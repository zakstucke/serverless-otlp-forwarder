//! AWS Lambda function that forwards CloudWatch log wrapped OTLP records to OpenTelemetry collectors.
//!
//! This Lambda function:
//! 1. Receives CloudWatch log events as otlp-stout format
//! 2. Decodes and decompresses the log data
//! 3. Converts logs to TelemetryData
//! 4. Forwards the data to collectors in parallel
//!
//! The function supports:
//! - Multiple collectors with different endpoints
//! - Custom headers and authentication
//! - Base64 encoded payloads
//! - Gzip compressed data
//! - OpenTelemetry instrumentation

use anyhow::{Context, Result};
use aws_lambda_events::event::cloudwatch_logs::{LogEntry, LogsEvent};
use aws_sdk_secretsmanager::Client as SecretsManagerClient;
use opentelemetry::Value;
use reqwest::Client as ReqwestClient;
use std::collections::HashMap;
use std::sync::Arc;
use tracing;

use aws_credential_types::{provider::ProvideCredentials, Credentials};
use lambda_otlp_forwarder::{
    collectors::Collectors,
    processing::process_telemetry_batch,
    span_compactor::{compact_telemetry_payloads, SpanCompactionConfig},
    telemetry::TelemetryData,
};
use otlp_sigv4_client::SigV4ClientBuilder;

use lambda_otel_lite::{
    init_telemetry, OtelTracingLayer, SpanAttributes, SpanAttributesExtractor, TelemetryConfig,
};

use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::trace::BatchSpanProcessor;

use lambda_runtime::{tower::ServiceBuilder, Error as LambdaError, LambdaEvent, Runtime};
use serde::{Deserialize, Serialize};
/// Shared application state across Lambda invocations
struct AppState {
    http_client: ReqwestClient,
    credentials: Credentials,
    secrets_client: SecretsManagerClient,
    region: String,
}

impl AppState {
    async fn new() -> Result<Self, LambdaError> {
        let config = aws_config::load_from_env().await;
        let credentials = config
            .credentials_provider()
            .expect("No credentials provider found")
            .provide_credentials()
            .await?;
        let region = config.region().expect("No region found").to_string();

        Ok(Self {
            http_client: ReqwestClient::new(),
            credentials,
            secrets_client: SecretsManagerClient::new(&config),
            region,
        })
    }
}

/// Convert a CloudWatch log event into TelemetryData
fn convert_log_event(event: &LogEntry) -> Result<TelemetryData> {
    let record = &event.message;

    // Parse as a standard LogRecord
    let log_record = serde_json::from_str(record)
        .with_context(|| format!("Failed to parse log record: {}", record))?;

    // Convert to TelemetryData (will be in uncompressed protobuf format)
    TelemetryData::from_log_record(log_record)
}

async fn function_handler(
    event: LambdaEvent<LogsEventWrapper>,
    state: Arc<AppState>,
) -> Result<(), LambdaError> {
    tracing::debug!("Function handler started");

    // Check and refresh collectors cache if stale
    Collectors::init(&state.secrets_client).await?;

    let log_events = event.payload.0.aws_logs.data.log_events;

    // Convert all events to TelemetryData (sequentially)
    let telemetry_batch: Vec<TelemetryData> = log_events
        .iter()
        .filter_map(|event| match convert_log_event(event) {
            Ok(telemetry) => Some(telemetry),
            Err(e) => {
                tracing::warn!("Failed to convert span event: {}", e);
                None
            }
        })
        .collect();
    
    // If we have telemetry data, process it
    if !telemetry_batch.is_empty() {
        // Compact multiple payloads into a single one
        // This will also apply compression to the final result
        let compacted_telemetry = match compact_telemetry_payloads(telemetry_batch, &SpanCompactionConfig::default()) {
            Ok(telemetry) => vec![telemetry],
            Err(e) => {
                tracing::error!("Failed to compact telemetry payloads: {}", e);
                return Err(e);
            }
        };
        
        // Process the compacted telemetry (single POST request)
        process_telemetry_batch(
            compacted_telemetry,
            &state.http_client,
            &state.credentials,
            &state.region,
        )
        .await?;
    }
    
    Ok(())
}

// Wrapper type for LogsEvent to implement SpanAttributesExtractor
#[derive(Clone, Debug, Serialize, Deserialize)]
struct LogsEventWrapper(LogsEvent);

impl SpanAttributesExtractor for LogsEventWrapper {
    fn extract_span_attributes(&self) -> SpanAttributes {
        let mut attributes: HashMap<String, Value> = HashMap::new();
        let log_data = self.0.aws_logs.data.clone();
        // Add any attributes you want to extract from LogsEvent
        attributes.insert(
            "forwarder.log_group".to_string(),
            Value::String(log_data.log_group.clone().into()),
        );
        attributes.insert(
            "forwarder.events.count".to_string(),
            Value::I64(log_data.log_events.len() as i64),
        );

        SpanAttributes::builder()
            .span_name(log_data.log_group)
            .kind("consumer".to_string())
            .attributes(attributes)
            .build()
    }
}

#[tokio::main]
async fn main() -> Result<(), LambdaError> {
    let config = aws_config::load_from_env().await;
    let region = config.region().expect("No region found");
    let credentials = config
        .credentials_provider()
        .expect("No credentials provider found")
        .provide_credentials()
        .await?;

    let sigv4_client = SigV4ClientBuilder::new()
        .with_client(
            reqwest::blocking::Client::builder()
                .build()
                .map_err(|e| LambdaError::from(format!("Failed to build HTTP client: {}", e)))?,
        )
        .with_credentials(credentials)
        .with_region(region.to_string())
        .with_service("xray")
        .with_signing_predicate(Box::new(|request| {
            // Only sign requests to AWS endpoints
            request
                .uri()
                .host()
                .map_or(false, |host| host.ends_with(".amazonaws.com"))
        }))
        .build()?;

    // Create a new exporter for BatchSpanProcessor
    let batch_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(sigv4_client)
        .with_protocol(Protocol::HttpBinary)
        .with_timeout(std::time::Duration::from_secs(3))
        .build()?;

    let (_, completion_handler) = init_telemetry(
        TelemetryConfig::builder()
            .with_span_processor(BatchSpanProcessor::builder(batch_exporter).build())
            // .enable_fmt_layer(true)
            .build(),
    )
    .await?;

    // Initialize shared application state
    let state = Arc::new(AppState::new().await?);

    // Initialize collectors using state's secrets client
    Collectors::init(&state.secrets_client).await?;

    let service = ServiceBuilder::new()
        .layer(OtelTracingLayer::new(completion_handler))
        .service_fn(|event| {
            let state = Arc::clone(&state);
            async move { function_handler(event, state).await }
        });

    // Create and run the Lambda runtime
    let runtime = Runtime::new(service);
    runtime.run().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_convert_log_event() {
        // Test standard LogRecord with valid OTLP structure
        let log_record = json!({
            "__otel_otlp_stdout": "otlp-stdout-client@0.2.2",
            "source": "test-service",
            "endpoint": "http://example.com",
            "method": "POST",
            "payload": {"resourceSpans": []}, // Valid OTLP JSON structure
            "headers": {
                "content-type": "application/json"
            },
            "content-type": "application/json",
            "content-encoding": null, // No compression in the test
            "base64": false
        });

        let event = LogEntry {
            id: "test-id".to_string(),
            timestamp: 1234567890,
            message: serde_json::to_string(&log_record).unwrap(),
        };

        let result = convert_log_event(&event);
        if let Err(e) = &result {
            println!("Error converting log event: {}", e);
        }
        assert!(result.is_ok());
        let telemetry = result.unwrap();
        assert_eq!(telemetry.source, "test-service");
        assert_eq!(telemetry.content_type, "application/x-protobuf"); // Now converted to protobuf
        assert_eq!(telemetry.content_encoding, None); // No compression at this stage
    }
}
