//! One poll: find the mail that reached a watched alias and send it on.
//!
//! HEY's "Everything" view lists threads by latest activity. A poll walks it back to a
//! horizon, reads the entries of each thread that moved, and looks at every message it has
//! not dealt with yet: one addressed to a watched alias is forwarded the way HEY's own
//! Forward button would, and every message looked at is remembered so it is never looked
//! at — or sent — again.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use hey_sdk::models::{Entry, Message, Topic};
use hey_sdk::services::MessageContent;
use hey_sdk::{Client, ErrorCode};
use tracing::{debug, error, info, warn};

use crate::config::{Config, Match};
use crate::state::StateFile;

/// How far before the last poll a quick poll looks, to absorb clock skew and mail HEY was
/// still filing when that poll ran.
const OVERLAP: TimeDelta = TimeDelta::minutes(15);
/// The most pages of a listing one poll reads before giving up on reaching its end.
const MAX_PAGES: usize = 40;

pub struct Forwarder {
    client: Client,
    config: Config,
    lookback: TimeDelta,
    /// Finds what would be forwarded without sending any of it.
    dry_run: bool,
    /// Threads already read through, by the activity they showed then. One is not read
    /// again until it moves.
    read_topics: HashMap<i64, DateTime<Utc>>,
    /// Threads holding a message HEY refused to forward; a deep poll tries them again.
    retry_topics: HashSet<i64>,
}

/// What one poll did.
#[derive(Debug, Default)]
pub struct Report {
    /// Threads whose entries were read.
    pub topics: usize,
    /// Messages fetched and held against the rules.
    pub messages: usize,
    /// Messages sent on — or, on a dry run, that would have been.
    pub forwarded: Vec<Forwarded>,
    /// Messages HEY refused to forward; they are tried again later.
    pub failed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forwarded {
    pub entry_id: i64,
    pub received_at: DateTime<Utc>,
    pub subject: String,
    pub aliases: Vec<String>,
    pub to: Vec<String>,
}

enum Outcome {
    /// Dealt with for good: forwarded, or nothing to forward.
    Done,
    /// Left alone, and not worth remembering either.
    TooOld,
    /// Could not be forwarded this time.
    Failed,
}

impl Forwarder {
    pub fn new(client: Client, config: Config, dry_run: bool) -> Forwarder {
        let lookback = TimeDelta::from_std(config.lookback).unwrap_or(TimeDelta::MAX);
        Forwarder {
            client,
            config,
            lookback,
            dry_run,
            read_topics: HashMap::new(),
            retry_topics: HashSet::new(),
        }
    }

    /// Runs one poll at `now`.
    ///
    /// A quick poll reads the threads that moved since the last one. A `deep` poll reads
    /// back through the whole lookback window, which is what picks up mail that surfaced
    /// late and retries what HEY refused earlier.
    ///
    /// An error means the poll was cut short — HEY unreachable, the sign-in refused — and
    /// nothing is lost by it: the next poll starts from where the last complete one did.
    pub async fn poll(
        &mut self,
        file: &mut StateFile,
        now: DateTime<Utc>,
        deep: bool,
    ) -> Result<Report> {
        let since_last = file.state.checkpoint - OVERLAP;
        // Nothing older than this is forwarded: not mail from before the first run, and not
        // mail from before the lookback window, unless the daemon was down for longer.
        let floor = (now - self.lookback)
            .min(since_last)
            .max(file.state.baseline);
        let horizon = if deep { floor } else { since_last.max(floor) };

        let mut report = Report::default();
        let mut page = self
            .client
            .topics()
            .get_everything(&Default::default())
            .await?;
        for pages_read in 1.. {
            let topics = page.topics.as_deref().unwrap_or_default();
            for topic in topics {
                if activity(topic).is_none_or(|at| at >= horizon) {
                    self.read_topic(topic, file, now, floor, deep, &mut report)
                        .await?;
                }
            }
            // The view is ordered by activity, so a page ending past the horizon is the
            // last one worth reading.
            let past_horizon = topics
                .last()
                .is_none_or(|last| activity(last).is_some_and(|at| at < horizon));
            if past_horizon {
                break;
            }
            if pages_read >= MAX_PAGES {
                warn!("stopped after {MAX_PAGES} pages of threads without reaching {horizon}");
                break;
            }
            match self.client.next_page(&page).await? {
                Some(next) => page = next,
                None => break,
            }
        }

        self.read_topics.retain(|_, at| *at >= floor);
        self.retry_topics
            .retain(|id| self.read_topics.contains_key(id));
        file.state.checkpoint = now;
        file.state.prune(floor);
        file.save()?;
        Ok(report)
    }

    async fn read_topic(
        &mut self,
        topic: &Topic,
        file: &mut StateFile,
        now: DateTime<Utc>,
        floor: DateTime<Utc>,
        deep: bool,
        report: &mut Report,
    ) -> Result<()> {
        let activity = activity(topic);
        let unchanged = activity.is_some() && self.read_topics.get(&topic.id) == activity.as_ref();
        if unchanged && !(deep && self.retry_topics.contains(&topic.id)) {
            return Ok(());
        }

        report.topics += 1;
        let mut failed = false;
        for entry in self.entries(topic.id).await? {
            let handled = file.state.handled.contains_key(&entry.id);
            let too_old = entry.created_at.is_some_and(|created| created < floor);
            // A note on a thread is an entry too, but HEY never mails those.
            let note = entry.kind.as_deref() == Some("comment");
            if handled || too_old || note {
                continue;
            }
            match self.examine(&entry, file, now, floor, report).await? {
                Outcome::Done => {
                    file.state.handled.insert(entry.id, now);
                }
                Outcome::TooOld => {}
                Outcome::Failed => failed = true,
            }
        }

        if let Some(activity) = activity {
            self.read_topics.insert(topic.id, activity);
        }
        if failed {
            self.retry_topics.insert(topic.id);
        } else {
            self.retry_topics.remove(&topic.id);
        }
        Ok(())
    }

    async fn entries(&self, topic_id: i64) -> Result<Vec<Entry>> {
        let mut page = self
            .client
            .topics()
            .get_entries(topic_id, &Default::default())
            .await?;
        let mut entries = page.value().clone();
        for _ in 1..MAX_PAGES {
            match self.client.next_page(&page).await? {
                Some(next) => {
                    entries.extend(next.iter().cloned());
                    page = next;
                }
                None => break,
            }
        }
        Ok(entries)
    }

    async fn examine(
        &self,
        entry: &Entry,
        file: &mut StateFile,
        now: DateTime<Utc>,
        floor: DateTime<Utc>,
        report: &mut Report,
    ) -> Result<Outcome> {
        let message = match self.client.messages().get(entry.id).await {
            Ok(message) => message,
            // An entry that is not a message — nothing to forward.
            Err(error) if error.code() == ErrorCode::NotFound => return Ok(Outcome::Done),
            Err(error) => return Err(error.into()),
        };
        let Some(received_at) = entry.created_at.or(message.created_at) else {
            // Without a date there is no telling new mail from old, and old mail must
            // never go out.
            warn!(entry = entry.id, "skipped a message HEY gave no date for");
            return Ok(Outcome::Done);
        };
        if received_at < floor {
            return Ok(Outcome::TooOld);
        }

        report.messages += 1;
        let recipients = recipients(&message);
        debug!(entry = entry.id, ?recipients, "examined a message");
        let Some(matched) = self.config.matching(recipients.iter().map(String::as_str)) else {
            return Ok(Outcome::Done);
        };

        if !self.dry_run {
            if let Err(error) = self.forward(entry.id, &message, &matched).await {
                // HEY out of reach or the sign-in refused: the whole poll is cut short and
                // run again, rather than this one message set aside.
                if error.is_retryable() || error.code() == ErrorCode::Auth {
                    return Err(error.into());
                }
                error!(
                    entry = entry.id,
                    aliases = ?matched.aliases,
                    "HEY refused the forward, will try again later: {error}"
                );
                report.failed += 1;
                return Ok(Outcome::Failed);
            }
            // Remembered at once, so that a crash before the poll ends cannot send it twice.
            file.state.handled.insert(entry.id, now);
            file.save()?;
            info!(
                entry = entry.id,
                aliases = ?matched.aliases,
                to = ?matched.to,
                "forwarded"
            );
        }
        report.forwarded.push(Forwarded {
            entry_id: entry.id,
            received_at,
            subject: message.subject.unwrap_or_default(),
            aliases: matched.aliases,
            to: matched.to,
        });
        Ok(Outcome::Done)
    }

    /// Sends a message on as HEY's own Forward would: its prefilled subject and quoted
    /// body, addressed to the rule's targets.
    async fn forward(
        &self,
        entry_id: i64,
        message: &Message,
        matched: &Match,
    ) -> Result<(), hey_sdk::Error> {
        let draft = self.client.entries().new_forward(entry_id).await?;
        let subject = draft.subject.filter(|subject| !subject.is_empty());
        let content = draft.content.filter(|content| !content.is_empty());
        self.client
            .messages()
            .send(&MessageContent {
                subject: subject.unwrap_or_else(|| {
                    format!("Fwd: {}", message.subject.as_deref().unwrap_or_default())
                }),
                content: content
                    .or_else(|| message.content.clone())
                    .unwrap_or_default(),
                to: matched.to.clone(),
                ..Default::default()
            })
            .await?;
        Ok(())
    }
}

/// When a thread last moved, by the best account of it HEY gives.
fn activity(topic: &Topic) -> Option<DateTime<Utc>> {
    topic
        .active_at
        .or_else(|| topic.latest_entry.as_ref()?.created_at)
        .or(topic.updated_at)
        .or(topic.created_at)
}

/// Every address a message was sent to: the account addresses HEY recorded it arriving
/// through, and everyone on its To, Cc and Bcc lines.
fn recipients(message: &Message) -> Vec<String> {
    let via = message
        .received_via
        .iter()
        .flatten()
        .map(|via| via.email_address.expose().to_string());
    let addressed = message
        .addressed
        .iter()
        .flat_map(|addressed| {
            [
                &addressed.directly,
                &addressed.copied,
                &addressed.blindcopied,
            ]
        })
        .flatten()
        .flatten()
        .filter_map(|contact| contact.email_address.as_ref())
        .map(|address| address.expose().to_string());
    via.chain(addressed).collect()
}
