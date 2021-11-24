use crate::{
    codecs::{DecodingConfig, FramingConfig, ParserConfig},
    config::{
        AcknowledgementsConfig, DataType, GenerateConfig, Resource, SourceConfig, SourceContext,
    },
    serde::{bool_or_struct, default_decoding, default_framing_message_based},
    sources,
    sources::datadog::agent::DatadogAgentSource,
    sources::util::ErrorMessage,
    tls::{MaybeTlsSettings, TlsConfig},
};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use warp::{reject::Rejection, Filter};

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DatadogAgentConfig {
    address: SocketAddr,
    tls: Option<TlsConfig>,
    #[serde(default = "crate::serde::default_true")]
    store_api_key: bool,

    // Those are probably useless as we know input encoding
    #[serde(default = "default_framing_message_based")]
    framing: Box<dyn FramingConfig>,
    #[serde(default = "default_decoding")]
    decoding: Box<dyn DeserializerConfig>,
    #[serde(default, deserialize_with = "bool_or_struct")]
    acknowledgements: AcknowledgementsConfig,
    #[serde(default = "crate::serde::default_true")]
    accept_logs: bool,
    #[serde(default = "crate::serde::default_true")]
    accept_traces: bool,
}

impl GenerateConfig for DatadogAgentConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(Self {
            address: "0.0.0.0:8080".parse().unwrap(),
            tls: None,
            store_api_key: true,
            framing: default_framing_message_based(),
            decoding: default_decoding(),
            acknowledgements: AcknowledgementsConfig::default(),
            accept_logs: true,
            accept_traces: true,
        })
        .unwrap()
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "datadog_agent")]
impl SourceConfig for DatadogAgentConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<sources::Source> {
        let decoder = DecodingConfig::new(self.framing.clone(), self.decoding.clone()).build()?;
        let source = DatadogAgentSource::new(
            self.acknowledgements.enabled,
            cx.out,
            self.store_api_key,
            decoder,
        );

        let tls = MaybeTlsSettings::from_config(&self.tls, true)?;
        let listener = tls.bind(&self.address).await?;

        let filters = source.build_warp_filters(self.accept_logs, self.accept_traces, true)?;

        let shutdown = cx.shutdown;
        Ok(Box::pin(async move {
            let span = crate::trace::current_span();
            let routes = filters
                .with(warp::trace(move |_info| span.clone()))
                .recover(|r: Rejection| async move {
                    if let Some(e_msg) = r.find::<ErrorMessage>() {
                        let json = warp::reply::json(e_msg);
                        Ok(warp::reply::with_status(json, e_msg.status_code()))
                    } else {
                        // other internal error - will return 500 internal server error
                        Err(r)
                    }
                });
            warp::serve(routes)
                .serve_incoming_with_graceful_shutdown(
                    listener.accept_stream(),
                    shutdown.map(|_| ()),
                )
                .await;

            Ok(())
        }))
    }

    fn output_type(&self) -> DataType {
        DataType::Any
    }

    fn source_type(&self) -> &'static str {
        "datadog_agent"
    }

    fn resources(&self) -> Vec<Resource> {
        vec![Resource::tcp(self.address)]
    }
}
