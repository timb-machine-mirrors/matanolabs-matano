use std::collections::HashMap;
use std::io::Write;

use anyhow::{anyhow, Context as AnyhowContext, Error, Result};
use async_once::AsyncOnce;
use aws_config::SdkConfig;
use aws_lambda_events::event::sqs::SqsEvent;
use aws_sdk_s3::types::ByteStream;
use futures::stream::FuturesOrdered;
use futures::{FutureExt, TryFutureExt};
use futures_util::stream::StreamExt;
use lambda_runtime::{run, service_fn, Error as LambdaError, LambdaEvent};
use lazy_static::lazy_static;
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use shared::setup_logging;
use walkdir::WalkDir;

mod pullers;
use pullers::{LogSource, PullLogs, PullLogsContext};

lazy_static! {
    static ref REQ_CLIENT: reqwest::Client = reqwest::Client::new();
    static ref CONTEXTS: HashMap<String, PullLogsContext> = build_contexts();
    static ref AWS_CONFIG: AsyncOnce<SdkConfig> =
        AsyncOnce::new(async { aws_config::load_from_env().await });
    static ref S3_CLIENT: AsyncOnce<aws_sdk_s3::Client> =
        AsyncOnce::new(async { aws_sdk_s3::Client::new(AWS_CONFIG.get().await) });
    static ref SECRETS_CLIENT: AsyncOnce<aws_sdk_secretsmanager::Client> =
        AsyncOnce::new(async { aws_sdk_secretsmanager::Client::new(AWS_CONFIG.get().await) });
}

fn build_contexts() -> HashMap<String, PullLogsContext> {
    let puller_log_source_types: Vec<String> =
        serde_json::from_str(&std::env::var("PULLER_LOG_SOURCE_TYPES").unwrap()).unwrap();
    let secret_arns: HashMap<String, String> =
        serde_json::from_str(&std::env::var("SECRET_ARNS").unwrap()).unwrap();

    let ret = WalkDir::new("/opt/config/log_sources")
        .min_depth(1)
        .max_depth(1)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let p = e.path().to_owned();
            p.is_dir().then_some(p)
        })
        .flat_map(|log_source_dir_path| {
            let ls_config_path = log_source_dir_path.join("log_source.yml");
            let ls_config_path = ls_config_path.as_path().to_str().unwrap();

            let file = std::fs::File::open(ls_config_path).unwrap();
            let config: serde_yaml::Value = serde_yaml::from_reader(file).unwrap();

            let ls_name = config
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let managed_type = config
                .get("managed")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let managed_properties = config
                .get("managed")
                .and_then(|v| v.get("properties"))
                .and_then(|v| v.as_mapping())
                .map(|v| v.to_owned());

            let log_source = ls_name.as_ref().and_then(|lsn| LogSource::from_str(lsn));

            Some((ls_name?, log_source?, managed_type?, managed_properties?))
        })
        .filter(|(_, _, managed_type, _)| puller_log_source_types.iter().any(|s| s == managed_type))
        .map(|(ls_name, log_source, managed_type, managed_properties)| {
            let mut props = managed_properties
                .into_iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_string(), v.as_str()?.to_string())))
                .collect::<HashMap<_, _>>();
            props.insert("log_source_type".to_string(), managed_type);

            let secret_arn = secret_arns
                .get(&ls_name)
                .context("Need secret arn.")
                .unwrap();

            let ctx = PullLogsContext::new(secret_arn.to_owned(), log_source, props);

            (ls_name.to_string(), ctx)
        })
        .collect::<HashMap<_, _>>();
    ret
}

#[tokio::main]
async fn main() -> Result<(), LambdaError> {
    setup_logging();

    let func = service_fn(handler);
    run(func).await?;

    Ok(())
}

#[derive(Serialize, Deserialize, Debug)]
struct PullerRequest {
    log_source_name: String,
    time: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct SQSBatchResponseItemFailure {
    itemIdentifier: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct SQSBatchResponse {
    batchItemFailures: Vec<SQSBatchResponseItemFailure>,
}

async fn handler(event: LambdaEvent<SqsEvent>) -> Result<Option<SQSBatchResponse>> {
    info!("Starting....");
    let client = REQ_CLIENT.clone();
    let contexts = &CONTEXTS;

    let mut failures = vec![];

    let records = event
        .payload
        .records
        .into_iter()
        .flat_map(|msg| msg.body.and_then(|b| Some((msg.message_id.unwrap(), b))))
        .flat_map(|(id, body)| {
            serde_json::from_str::<PullerRequest>(&body).and_then(|b| Ok((id, b)))
        })
        .collect::<Vec<_>>();

    let (msg_ids, records): (Vec<_>, Vec<_>) = records.into_iter().unzip();

    info!("Processing {} messages.", records.len());

    let futs = records
        .into_iter()
        .map(|record| {
            let ctx = contexts
                .get(&record.log_source_name)
                .context("Invalid log source.")?;

            let puller = ctx.log_source_type.clone();
            let log_source_name = record.log_source_name.clone();
            let fut = puller
                .pull_logs(client.clone(), ctx)
                .and_then(|data| async move { upload_data(data, &record.log_source_name).await })
                .map(move |r| {
                    r.with_context(|| format!("Error for log_source: {}", log_source_name))
                });
            anyhow::Ok(fut)
        })
        .zip(msg_ids.iter())
        .filter_map(|(r, msg_id)| {
            r.map_err(|e| {
                error!("{:?}", e);
                failures.push(SQSBatchResponseItemFailure {
                    itemIdentifier: msg_id.to_string(),
                });
                e
            })
            .ok()
            .and_then(|r| Some((msg_id, r)))
        });
    let (msg_ids, futs): (Vec<_>, Vec<_>) = futs.unzip();

    let results = futs
        .into_iter()
        .collect::<FuturesOrdered<_>>()
        .collect::<Vec<_>>()
        .await;

    for (result, msg_id) in results.into_iter().zip(msg_ids) {
        match result {
            Ok(_) => (),
            Err(e) => {
                error!("Failed: {}", e);
                failures.push(SQSBatchResponseItemFailure {
                    itemIdentifier: msg_id.to_owned(),
                });
            }
        };
    }

    if failures.is_empty() {
        Ok(None)
    } else {
        error!(
            "Encountered {} errors processing messages, returning to SQS",
            failures.len()
        );
        Ok(Some(SQSBatchResponse {
            batchItemFailures: failures,
        }))
    }
}

async fn upload_data(data: Vec<u8>, log_source: &str) -> Result<()> {
    if data.is_empty() {
        info!("No new data for log_source: {}", log_source);
        return Ok(());
    }
    info!("Uploading data for {}", log_source);
    let bucket = std::env::var("INGESTION_BUCKET_NAME")?;
    let key = format!(
        "{}/{}.json.zst",
        log_source,
        uuid::Uuid::new_v4().to_string()
    );
    let s3 = S3_CLIENT.get().await;
    info!("Writing to s3://{}/{}", bucket, key);

    let mut zencoder = zstd::Encoder::new(vec![], 0)?;
    zencoder.write_all(data.as_slice())?;
    let final_data = zencoder.finish()?;

    s3.put_object()
        .bucket(&bucket)
        .key(&key)
        .body(ByteStream::from(final_data))
        .content_encoding("application/zstd".to_string())
        .send()
        .await
        .map_err(|e| {
            error!("Error putting {} to S3: {}", key, e);
            e
        })?;

    Ok(())
}