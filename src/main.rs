use anyhow::{Context, Result};
use axum::{
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use byte_unit::Byte;
use clap::Parser;
use http::{header, status::StatusCode};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

use std::{net::SocketAddr, path::PathBuf};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    serve(&cli.listen).await;
    Ok(())
}

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = "[::1]:22903")]
    listen: SocketAddr,
}

pub(crate) async fn serve(listen: &SocketAddr) {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "example_form=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // build our application with some routes
    let app = Router::new().route("/metrics", get(http_metrics));

    // run it
    let listener = tokio::net::TcpListener::bind(listen).await.unwrap();
    tracing::debug!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

struct AppError(anyhow::Error);

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

// Tell axum how to convert `AppError` into a response.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        eprintln!("{:#?}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {:#}", self.0),
        )
            .into_response()
    }
}

async fn http_metrics() -> std::result::Result<impl IntoResponse, AppError> {
    let mut out = String::new();
    for metric in get_metrics()? {
        out += &metric.encode();
    }

    Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out))
}

struct Metric {
    name: &'static str,
    labels: Labels,
    value: f64,
}
type Labels = Vec<(&'static str, String)>;

impl Metric {
    fn encode(&self) -> String {
        let labels = Self::encode_labels(&self.labels);
        format!(
            "{name}{{{labels}}} {value}\n",
            name = self.name,
            value = self.value
        )
    }

    fn encode_labels(labels: &[(&'static str, String)]) -> String {
        let mut out = String::new();
        for (key, value) in labels {
            out.push_str(key);
            out.push_str("=\"");
            out.push_str(
                &value
                    .replace('\\', r#"\\"#)
                    .replace('\n', r#"\n"#)
                    .replace('"', r#"\""#),
            );
            out.push_str("\",");
        }
        out
    }
}

fn get_metrics() -> Result<Vec<Metric>> {
    let mut metrics = Vec::new();
    for fs in find_bcachefs()? {
        metrics.append(&mut fs.get_metrics()?);
    }
    Ok(metrics)
}

const SYSFS_BCACHEFS_ROOT: &str = "/sys/fs/bcachefs";
fn find_bcachefs() -> Result<Vec<Fs>> {
    let mut fs = Vec::new();
    for entry in PathBuf::from(SYSFS_BCACHEFS_ROOT).read_dir()? {
        fs.push(Fs(Uuid::parse_str(entry?.file_name().to_str().unwrap())?));
    }
    Ok(fs)
}

#[derive(Debug)]
struct Fs(Uuid);
impl Fs {
    fn get_metrics(&self) -> Result<Vec<Metric>> {
        let mut metrics = Vec::new();
        for device in self.find_devices()? {
            metrics.append(&mut device.get_metrics()?);
        }
        Ok(metrics)
    }
    fn path(&self) -> PathBuf {
        PathBuf::from(SYSFS_BCACHEFS_ROOT).join(self.0.to_string())
    }

    fn find_devices(&self) -> Result<Vec<Device>> {
        let mut devices = Vec::new();
        for entry in self.path().read_dir()? {
            let entry = entry?;
            let file_name = entry.file_name();
            let file_name = file_name.to_str().unwrap();
            if !file_name.starts_with("dev-") {
                continue;
            }
            let device_no: usize = file_name
                .strip_prefix("dev-")
                .unwrap()
                .parse()
                .with_context(|| format!("file_name={file_name:?}"))?;
            devices.push(Device {
                fs: self,
                device_no,
            });
        }
        Ok(devices)
    }
}
#[derive(Debug)]
struct Device<'a> {
    fs: &'a Fs,
    device_no: usize,
}
impl Device<'_> {
    fn get_metrics(&self) -> Result<Vec<Metric>> {
        let mut metrics = Vec::new();
        let device_name = self
            .path()
            .join("block")
            .read_link()?
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let label = std::fs::read_to_string(self.path().join("label"))
            .with_context(|| "reading dev-$x/label")?
            .trim()
            .to_string();
        let device_labels = vec![
            ("fs", self.fs.0.to_string()),
            ("device_no", self.device_no.to_string()),
            ("device", device_name),
            ("label", label),
        ];
        metrics.append(&mut self.alloc_debug(&device_labels)?);
        Ok(metrics)
    }

    fn path(&self) -> PathBuf {
        self.fs.path().join(format!("dev-{}", self.device_no))
    }

    fn alloc_debug(&self, device_labels: &Labels) -> Result<Vec<Metric>> {
        let mut metrics = Vec::new();
        let s = std::fs::read_to_string(self.path().join("alloc_debug"))?;
        let mut lines = s.lines();
        let header: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
        assert_eq!(header, ["buckets", "sectors", "fragmented"]);
        for line in lines {
            if line.is_empty() {
                break;
            }
            let cells: Vec<_> = line.split_whitespace().collect();
            match cells[..] {
                [type_, _buckets, sectors, _fragmented] => {
                    let mut labels = device_labels.clone();
                    labels.push(("type", type_.to_string()));
                    metrics.push(Metric {
                        name: "bcachefs_dev_alloc_bytes",
                        labels,
                        value: sectors_to_bytes(sectors)?,
                    });
                }
                ["capacity", buckets] => {
                    metrics.push(Metric {
                        name: "bcachefs_dev_capacity",
                        labels: device_labels.clone(),
                        value: self.buckets_to_bytes(buckets)?,
                    });
                }
                _ => {
                    panic!("can't handle line {line}")
                }
            }
        }
        Ok(metrics)
    }

    fn bucket_size(&self) -> Result<u64> {
        let file_content = std::fs::read_to_string(self.path().join("bucket_size"))
            .with_context(|| "reading dev-$x/bucket_size")?;
        Ok(Byte::parse_str(&file_content, true)
            .with_context(|| format!("file_content={file_content:?}"))?
            .as_u64())
    }
    fn buckets_to_bytes(&self, sectors: &str) -> Result<f64> {
        let sectors: u64 = sectors
            .parse()
            .with_context(|| format!("sectors={sectors:?}"))?;
        let bucket_size = self.bucket_size()?;
        Ok((bucket_size * sectors) as f64)
    }
}

fn sectors_to_bytes(sectors: &str) -> Result<f64> {
    // Apparently the sectors are always 2<<9 = 512 bytes. Even when the disk runs with 4k sectors.
    Ok((sectors
        .parse::<usize>()
        .with_context(|| format!("sectors={sectors:?}"))?
        << 9) as f64)
}
