use crate::{
    common::datadog::{DatadogMetricType, DatadogSeriesMetric},
    config::log_schema,
    event::{metric::Metric, metric::MetricValue, Event, MetricKind},
    internal_events::EventsReceived,
    sources::datadog::agent::{decode, handle_request, ApiKeyExtractor, ApiKeyQueryParams},
    sources::datadog::sketch_parser::decode_ddsketch,
    sources::util::ErrorMessage,
    vector_core::ByteSizeOf,
    Pipeline,
};
use bytes::Bytes;
use chrono::{TimeZone, Utc};
use futures::future;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use warp::{filters::BoxedFilter, path, path::FullPath, reply::Response, Filter, Rejection};

#[derive(Deserialize, Serialize)]
struct DatadogSeriesRequest {
    series: Vec<DatadogSeriesMetric>,
}

pub(crate) fn build_warp_filter(
    acknowledgements: bool,
    out: Pipeline,
    api_key_extractor: ApiKeyExtractor,
) -> BoxedFilter<(Response,)> {
    let sketches_service =
        sketches_service(api_key_extractor.clone(), acknowledgements, out.clone());
    let series_v1_service = series_v1_service(api_key_extractor, acknowledgements, out.clone());
    let series_v2_service = series_v2_service();
    sketches_service
        .or(series_v1_service)
        .unify()
        .or(series_v2_service)
        .unify()
        .boxed()
}

fn sketches_service(
    api_key_extractor: ApiKeyExtractor,
    acknowledgements: bool,
    out: Pipeline,
) -> BoxedFilter<(Response,)> {
    warp::post()
        .and(path!("api" / "beta" / "sketches" / ..))
        .and(warp::path::full())
        .and(warp::header::optional::<String>("content-encoding"))
        .and(warp::header::optional::<String>("dd-api-key"))
        .and(warp::query::<ApiKeyQueryParams>())
        .and(warp::body::bytes())
        .and_then(
            move |path: FullPath,
                  encoding_header: Option<String>,
                  api_token: Option<String>,
                  query_params: ApiKeyQueryParams,
                  body: Bytes| {
                let events = decode(&encoding_header, body).and_then(|body| {
                    decode_datadog_sketches(
                        body,
                        api_key_extractor.extract(
                            path.as_str(),
                            api_token,
                            query_params.dd_api_key,
                        ),
                    )
                });
                handle_request(events, acknowledgements, out.clone())
            },
        )
        .boxed()
}

fn series_v1_service(
    api_key_extractor: ApiKeyExtractor,
    acknowledgements: bool,
    out: Pipeline,
) -> BoxedFilter<(Response,)> {
    warp::post()
        .and(path!("api" / "v1" / "series" / ..))
        .and(warp::path::full())
        .and(warp::header::optional::<String>("content-encoding"))
        .and(warp::header::optional::<String>("dd-api-key"))
        .and(warp::query::<ApiKeyQueryParams>())
        .and(warp::body::bytes())
        .and_then(
            move |path: FullPath,
                  encoding_header: Option<String>,
                  api_token: Option<String>,
                  query_params: ApiKeyQueryParams,
                  body: Bytes| {
                let events = decode(&encoding_header, body).and_then(|body| {
                    decode_datadog_series(
                        body,
                        api_key_extractor.extract(
                            path.as_str(),
                            api_token,
                            query_params.dd_api_key,
                        ),
                    )
                });
                handle_request(events, acknowledgements, out.clone())
            },
        )
        .boxed()
}

fn series_v2_service() -> BoxedFilter<(Response,)> {
    warp::post()
        // This should not happen anytime soon as the v2 series endpoint does not exist yet
        // but the route exists in the agent codebase
        .and(path!("api" / "v2" / "series" / ..))
        .and_then(|| {
            error!(message = "/api/v2/series route is not supported.");
            let response: Result<Response, Rejection> =
                Err(warp::reject::custom(ErrorMessage::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Vector does not support the /api/v2/series route".to_string(),
                )));
            future::ready(response)
        })
        .boxed()
}

fn decode_datadog_sketches(
    body: Bytes,
    api_key: Option<Arc<str>>,
) -> Result<Vec<Event>, ErrorMessage> {
    if body.is_empty() {
        // The datadog agent may send an empty payload as a keep alive
        debug!(
            message = "Empty payload ignored.",
            internal_log_rate_secs = 30
        );
        return Ok(Vec::new());
    }

    let metrics = decode_ddsketch(body, &api_key).map_err(|error| {
        ErrorMessage::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Error decoding Datadog sketch: {:?}", error),
        )
    })?;

    emit!(&EventsReceived {
        byte_size: metrics.size_of(),
        count: metrics.len(),
    });

    Ok(metrics)
}

fn decode_datadog_series(
    body: Bytes,
    api_key: Option<Arc<str>>,
) -> Result<Vec<Event>, ErrorMessage> {
    if body.is_empty() {
        // The datadog agent may send an empty payload as a keep alive
        debug!(
            message = "Empty payload ignored.",
            internal_log_rate_secs = 30
        );
        return Ok(Vec::new());
    }

    let metrics: DatadogSeriesRequest = serde_json::from_slice(&body).map_err(|error| {
        ErrorMessage::new(
            StatusCode::BAD_REQUEST,
            format!("Error parsing JSON: {:?}", error),
        )
    })?;

    let decoded_metrics: Vec<Event> = metrics
        .series
        .into_iter()
        .flat_map(|m| into_vector_metric(m, api_key.clone()))
        .collect();

    emit!(&EventsReceived {
        byte_size: decoded_metrics.size_of(),
        count: decoded_metrics.len(),
    });

    Ok(decoded_metrics)
}

fn into_vector_metric(dd_metric: DatadogSeriesMetric, api_key: Option<Arc<str>>) -> Vec<Event> {
    let mut tags: BTreeMap<String, String> = dd_metric
        .tags
        .unwrap_or_default()
        .iter()
        .map(|tag| {
            let kv = tag.split_once(":").unwrap_or((tag, ""));
            (kv.0.trim().into(), kv.1.trim().into())
        })
        .collect();

    dd_metric
        .host
        .and_then(|host| tags.insert(log_schema().host_key().to_owned(), host));
    dd_metric
        .source_type_name
        .and_then(|source| tags.insert("source_type_name".into(), source));
    dd_metric
        .device
        .and_then(|dev| tags.insert("device".into(), dev));

    match dd_metric.r#type {
        DatadogMetricType::Count => dd_metric
            .points
            .iter()
            .map(|dd_point| {
                Metric::new(
                    dd_metric.metric.clone(),
                    MetricKind::Incremental,
                    MetricValue::Counter { value: dd_point.1 },
                )
                .with_timestamp(Some(Utc.timestamp(dd_point.0, 0)))
                .with_tags(Some(tags.clone()))
            })
            .collect::<Vec<_>>(),
        DatadogMetricType::Gauge => dd_metric
            .points
            .iter()
            .map(|dd_point| {
                Metric::new(
                    dd_metric.metric.clone(),
                    MetricKind::Absolute,
                    MetricValue::Gauge { value: dd_point.1 },
                )
                .with_timestamp(Some(Utc.timestamp(dd_point.0, 0)))
                .with_tags(Some(tags.clone()))
            })
            .collect::<Vec<_>>(),
        // Agent sends rate only for dogstatsd counter https://github.com/DataDog/datadog-agent/blob/f4a13c6dca5e2da4bb722f861a8ac4c2f715531d/pkg/metrics/counter.go#L8-L10
        // for consistency purpose (w.r.t. (dog)statsd source) they are turned back into counters
        DatadogMetricType::Rate => dd_metric
            .points
            .iter()
            .map(|dd_point| {
                let i = dd_metric.interval.filter(|v| *v != 0).unwrap_or(1) as f64;
                Metric::new(
                    dd_metric.metric.clone(),
                    MetricKind::Incremental,
                    MetricValue::Counter {
                        value: dd_point.1 * i,
                    },
                )
                .with_timestamp(Some(Utc.timestamp(dd_point.0, 0)))
                .with_tags(Some(tags.clone()))
            })
            .collect::<Vec<_>>(),
    }
    .into_iter()
    .map(|mut metric| {
        if let Some(k) = &api_key {
            metric
                .metadata_mut()
                .set_datadog_api_key(Some(Arc::clone(k)));
        }
        metric.into()
    })
    .collect()
}
