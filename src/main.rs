use anyhow::Result;
use aws_config::meta::region::RegionProviderChain;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_ec2::types::{InstanceState, InstanceStatus, InstanceStatusSummary, Tag};
use aws_sdk_ec2::Client;
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use itertools::Itertools;
use lettre::{message::MultiPart, Message};
use query::portal::LoginPayload;
use rusoto_ses::Ses;
use rusoto_ses::{RawMessage, SendRawEmailRequest, SesClient};
use std::collections::HashMap;
use structopt::StructOpt;
use tokio::fs::OpenOptions;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
};

#[derive(Debug, StructOpt, Clone)]
struct Opt {
    #[structopt(short, long)]
    region: Option<String>,
    #[structopt(short, long, default_value = "https://api.fieldkit.org")]
    api: String,
    #[allow(dead_code)]
    #[structopt(short, long)]
    verbose: bool,
    #[structopt(short, long)]
    email: bool,
    #[structopt(short, long)]
    only_changes: bool,
}

#[derive(Default)]
struct Notification {
    messages: Vec<String>,
}

impl Notification {
    async fn send(self) -> Result<()> {
        if self.messages.is_empty() {
            return Ok(());
        }

        let ses_client = SesClient::new(rusoto_core::Region::UsEast1);

        let from = "FK <noreply@fieldkit.org>";
        let to = "Jacob Lewallen <jlewalle@gmail.com>";
        let subject = "FK Server Status";
        let body = self.messages.join("\n");

        send_email_ses(&ses_client, from, to, subject, body).await?;

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let options = Opt::from_args();

    let mut notification = Notification::default();

    match InstanceChecker::default().check(options.clone()).await {
        Ok(Some(m)) => notification.messages.push(m),
        Ok(None) => {}
        Err(e) => println!("{:?}", e),
    }

    match QueueChecker::default().check(&options).await {
        Ok(m) => notification.messages.extend(m),
        Err(e) => println!("{:?}", e),
    }

    notification.send().await?;

    Ok(())
}

async fn send_email_ses(
    ses_client: &SesClient,
    from: &str,
    to: &str,
    subject: &str,
    body: String,
) -> Result<()> {
    let email = Message::builder()
        .from(from.parse()?)
        .to(to.parse()?)
        .subject(subject)
        .multipart(MultiPart::alternative_plain_html(
            body.clone(),
            format!("<pre>{}</pre>", &body),
        ))?;

    let raw_email = email.formatted();

    let ses_request = SendRawEmailRequest {
        raw_message: RawMessage {
            data: BASE64_STANDARD.encode(raw_email).into(),
        },
        ..Default::default()
    };

    ses_client.send_raw_email(ses_request).await?;

    Ok(())
}

#[derive(Default)]
struct QueueChecker {}

impl QueueChecker {
    async fn check(&mut self, opts: &Opt) -> Result<Vec<String>> {
        let pc = query::portal::Client::new(&opts.api)?;
        let token = pc.login(LoginPayload::from_env()?).await?;
        let authed = pc.to_authenticated(token)?;
        let health = authed.query_admin_health().await?;

        println!("{:?}", health);

        let mut messages = Vec::new();

        if health.queue.pending > 500 {
            messages.push(format!("WARNING: Queue length is {}", health.queue.pending));
        }

        if health.queue.errors > 500 {
            messages.push(format!(
                "WARNING: Error queue length is {}",
                health.queue.errors
            ));
        }

        Ok(messages)
    }
}

#[derive(Default)]
struct InstanceChecker {}

impl InstanceChecker {
    async fn check(&mut self, opts: Opt) -> Result<Option<String>> {
        let Opt {
            region,
            verbose: _,
            api: _,
            email,
            only_changes,
        } = opts;

        let region_provider = RegionProviderChain::first_try(region.map(Region::new))
            .or_default_provider()
            .or_else(Region::new("us-east-1"));

        let shared_config = aws_config::defaults(BehaviorVersion::latest())
            .region(region_provider)
            .load()
            .await;
        let client = Client::new(&shared_config);

        let ids: Option<Vec<String>> = Some(vec![]);

        let servers = self.get_server_status(&client, ids).await?;

        let summaries: Vec<String> = servers
            .iter()
            .map(|server| {
                format!(
                    "{} {:20} {:20?} {:20?} {:20?}",
                    server.id,
                    server.name(),
                    server.state.as_ref().unwrap().name().unwrap(),
                    server.summary.as_ref().unwrap().status().unwrap(),
                    server.system_summary.as_ref().unwrap().status().unwrap()
                )
            })
            .collect();

        let paragraph = summaries.iter().join("\n");

        println!("{}", paragraph);

        let notifying = email && !only_changes;

        let modified = if only_changes {
            let state_path = "/tmp/monitor-state.txt";
            let modified = if let Some(previous) = self.read_previous_state(state_path).await {
                previous != paragraph
            } else {
                true
            };

            if modified {
                let mut options = OpenOptions::new();
                let mut file = options
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(state_path)
                    .await?;
                file.write_all(paragraph.as_bytes()).await?;
                file.flush().await?;
            }

            modified && email
        } else {
            false
        };

        if notifying || modified {
            Ok(Some(paragraph))
        } else {
            Ok(None)
        }
    }

    async fn get_server_status(
        &self,
        client: &Client,
        ids: Option<Vec<String>>,
    ) -> Result<Vec<ServerStatus>> {
        let instances_described = client
            .describe_instances()
            .set_instance_ids(ids.clone())
            .send()
            .await?;

        let instance_status_described = client
            .describe_instance_status()
            .set_instance_ids(ids.clone())
            .send()
            .await?;

        let described_status_by_id: HashMap<String, InstanceStatus> = instance_status_described
            .instance_statuses()
            .into_iter()
            .chunk_by(|r| r.instance_id().unwrap())
            .into_iter()
            .map(|(id, mut row)| (id.to_owned(), row.next().unwrap().clone()))
            .collect();

        let mut servers: Vec<ServerStatus> = Vec::new();

        for reservation in instances_described.reservations() {
            for instance in reservation.instances() {
                let id = instance.instance_id().unwrap();

                if let Some((_, status)) = described_status_by_id.get_key_value(id) {
                    servers.push(ServerStatus {
                        id: id.to_string(),
                        tags: instance.tags().into_iter().map(|r| r.clone()).collect(),
                        state: Some(status.instance_state().unwrap().clone()),
                        summary: Some(status.instance_status().unwrap().clone()),
                        system_summary: Some(status.system_status().unwrap().clone()),
                    });
                }
            }
        }

        Ok(servers)
    }

    async fn read_previous_state(&self, path: &str) -> Option<String> {
        if let Ok(mut file) = File::open(path).await {
            let mut buffer = String::new();
            if let Ok(_) = file.read_to_string(&mut buffer).await {
                Some(buffer)
            } else {
                None
            }
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct ServerStatus {
    id: String,
    tags: Vec<Tag>,
    state: Option<InstanceState>,
    summary: Option<InstanceStatusSummary>,
    system_summary: Option<InstanceStatusSummary>,
}

impl ServerStatus {
    pub fn name(&self) -> &str {
        for tag in self.tags.iter() {
            if tag.key() == Some("Name") {
                return tag.value().unwrap();
            }
        }

        "UNAMED"
    }
}
