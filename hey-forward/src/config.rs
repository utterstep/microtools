//! The TOML configuration: which aliases to watch and where their mail goes.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

const MIN_POLL_INTERVAL: Duration = Duration::from_secs(10);
const MIN_LOOKBACK: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// How often HEY is asked for new mail.
    #[serde(default = "default_poll_interval", with = "humantime_serde")]
    pub poll_interval: Duration,
    /// How far back mail is still picked up when it surfaces late: after the daemon was
    /// down, or once a sender is let out of the Screener.
    #[serde(default = "default_lookback", with = "humantime_serde")]
    pub lookback: Duration,
    #[serde(default, rename = "forward")]
    pub rules: Vec<Rule>,
}

/// Mail addressed to `alias` is forwarded to every address in `to`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub alias: String,
    pub to: Vec<String>,
}

/// What the rules say about one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The watched aliases the message was addressed to.
    pub aliases: Vec<String>,
    /// Everyone it goes on to, without repeats.
    pub to: Vec<String>,
}

fn default_poll_interval() -> Duration {
    Duration::from_secs(60)
}

fn default_lookback() -> Duration {
    Duration::from_secs(3 * 24 * 60 * 60)
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        Config::parse(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config> {
        let mut config: Config = toml::from_str(text)?;
        config.normalize()?;
        Ok(config)
    }

    fn normalize(&mut self) -> Result<()> {
        ensure!(
            self.poll_interval >= MIN_POLL_INTERVAL,
            "poll_interval must be at least {}",
            humantime::format_duration(MIN_POLL_INTERVAL)
        );
        ensure!(
            self.lookback >= MIN_LOOKBACK,
            "lookback must be at least {}",
            humantime::format_duration(MIN_LOOKBACK)
        );
        ensure!(
            !self.rules.is_empty(),
            "no [[forward]] rules: nothing to do"
        );

        let mut aliases = HashSet::new();
        for rule in &mut self.rules {
            rule.alias = address(&rule.alias)?;
            ensure!(
                aliases.insert(rule.alias.clone()),
                "alias {} appears in more than one rule",
                rule.alias
            );
            ensure!(
                !rule.to.is_empty(),
                "alias {} forwards to nobody",
                rule.alias
            );
            for target in &mut rule.to {
                *target = address(target)?;
            }
        }
        // A forward addressed to a watched alias would be forwarded again, forever.
        for rule in &self.rules {
            if let Some(target) = rule.to.iter().find(|target| aliases.contains(*target)) {
                bail!(
                    "alias {} forwards to {target}, which is itself a watched alias: that is a mail loop",
                    rule.alias
                );
            }
        }
        Ok(())
    }

    /// Matches a message's recipient addresses against the rules. Addresses compare
    /// case-insensitively, and a message that reached several aliases goes out once, to
    /// everyone those aliases name.
    pub fn matching<'a>(&self, recipients: impl IntoIterator<Item = &'a str>) -> Option<Match> {
        let recipients: HashSet<String> = recipients
            .into_iter()
            .map(|recipient| recipient.trim().to_lowercase())
            .collect();
        let mut matched = Match {
            aliases: Vec::new(),
            to: Vec::new(),
        };
        for rule in &self.rules {
            if recipients.contains(&rule.alias) {
                matched.aliases.push(rule.alias.clone());
                for target in &rule.to {
                    if !matched.to.contains(target) {
                        matched.to.push(target.clone());
                    }
                }
            }
        }
        (!matched.aliases.is_empty()).then_some(matched)
    }
}

/// Trims and lowercases an address, refusing anything that is plainly not one.
fn address(raw: &str) -> Result<String> {
    let address = raw.trim().to_lowercase();
    let plausible = match address.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.contains('@')
                && !address.contains(char::is_whitespace)
        }
        None => false,
    };
    ensure!(plausible, "{raw:?} is not an email address");
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_RULES: &str = r#"
        [[forward]]
        alias = "Me+Family@hey.com"
        to = ["partner@example.com"]

        [[forward]]
        alias = "me+bills@hey.com"
        to = ["partner@example.com", "books@example.com"]
    "#;

    #[test]
    fn defaults_apply_and_addresses_are_normalized() {
        let config = Config::parse(TWO_RULES).unwrap();
        assert_eq!(config.poll_interval, Duration::from_secs(60));
        assert_eq!(config.lookback, Duration::from_secs(259_200));
        assert_eq!(config.rules[0].alias, "me+family@hey.com");
    }

    #[test]
    fn durations_read_as_written() {
        let config = Config::parse(&format!(
            "poll_interval = \"2m\"\nlookback = \"12h\"\n{TWO_RULES}"
        ))
        .unwrap();
        assert_eq!(config.poll_interval, Duration::from_secs(120));
        assert_eq!(config.lookback, Duration::from_secs(43_200));
    }

    #[test]
    fn a_message_matches_its_alias_whatever_the_case() {
        let config = Config::parse(TWO_RULES).unwrap();
        let matched = config
            .matching(["someone@example.com", "ME+family@HEY.com"])
            .unwrap();
        assert_eq!(matched.aliases, ["me+family@hey.com"]);
        assert_eq!(matched.to, ["partner@example.com"]);
    }

    #[test]
    fn two_aliases_on_one_message_merge_their_targets() {
        let config = Config::parse(TWO_RULES).unwrap();
        let matched = config
            .matching(["me+bills@hey.com", "me+family@hey.com"])
            .unwrap();
        assert_eq!(matched.to, ["partner@example.com", "books@example.com"]);
    }

    #[test]
    fn the_plain_address_and_other_aliases_do_not_match() {
        let config = Config::parse(TWO_RULES).unwrap();
        assert_eq!(config.matching(["me@hey.com", "me+other@hey.com"]), None);
    }

    #[test]
    fn bad_configs_are_refused() {
        for (config, complaint) in [
            ("", "nothing to do"),
            (
                "[[forward]]\nalias = \"nope\"\nto = [\"a@example.com\"]",
                "not an email address",
            ),
            (
                "[[forward]]\nalias = \"me+a@hey.com\"\nto = []",
                "forwards to nobody",
            ),
            (
                "[[forward]]\nalias = \"me+a@hey.com\"\nto = [\"x@example.com\"]\n\
                 [[forward]]\nalias = \"ME+a@hey.com\"\nto = [\"y@example.com\"]",
                "more than one rule",
            ),
            (
                "[[forward]]\nalias = \"me+a@hey.com\"\nto = [\"me+b@hey.com\"]\n\
                 [[forward]]\nalias = \"me+b@hey.com\"\nto = [\"y@example.com\"]",
                "mail loop",
            ),
            (
                "poll_interval = \"1s\"\n[[forward]]\nalias = \"me+a@hey.com\"\nto = [\"x@example.com\"]",
                "poll_interval",
            ),
            (
                "[[forward]]\nalias = \"me+a@hey.com\"\nto = [\"x@example.com\"]\ncolour = \"red\"",
                "unknown field",
            ),
        ] {
            let error = format!("{:#}", Config::parse(config).unwrap_err());
            assert!(error.contains(complaint), "{error:?} lacks {complaint:?}");
        }
    }
}
