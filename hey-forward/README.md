# hey-forward

A small daemon that watches a [HEY](https://www.hey.com) mailbox and forwards mail sent
to chosen `+aliases` on to other addresses: `you+family@hey.com` to your partner,
`you+bills@hey.com` to your accountant.

HEY has no forwarding rules of its own. hey-forward polls the account through HEY's
official Rust SDK, [`hey-sdk`](https://crates.io/crates/hey-sdk), and sends each matching
message on the way the Forward button would, from your own address.

> **Status:** tested against a stand-in for the HEY API built from the SDK's wire
> formats, not yet against a live account. Run `hey-forward check` before trusting it.

## Setup

```sh
cargo build --release -p hey-forward           # → target/release/hey-forward

mkdir -p ~/.config/hey-forward
cp config.example.toml ~/.config/hey-forward/config.toml   # then edit the rules

hey-forward login    # sign in to HEY once
hey-forward check    # dry run: what would the rules have forwarded lately?
hey-forward run      # the daemon
```

`login` prints an address to open in a browser. On a headless machine, open it anywhere:
the browser ends up on a `127.0.0.1` page that fails to load, and pasting that page's
address back into the terminal completes the sign-in.

## Configuration

```toml
poll_interval = "1m"   # how often to look for new mail
lookback = "3d"        # how late mail may surface and still be forwarded

[[forward]]
alias = "you+family@hey.com"
to = ["partner@example.com"]

[[forward]]
alias = "you+bills@hey.com"
to = ["partner@example.com", "accountant@example.com"]
```

Both settings are optional and default to the values shown. An alias is matched as a
whole address, ignoring case. A rule may not forward to another watched alias, which
would loop.

## What to expect

- **Only new mail.** The first `run` notes the time, and nothing received before it is
  ever forwarded.
- **Matching.** A message matches when a watched alias is among the addresses HEY
  recorded it arriving through, or on its To or Cc line. One that reached several
  aliases goes out once, to everyone they name.
- **No repeats.** Every message looked at is remembered, so restarts send nothing twice.
  The one exception is a connection lost at the very moment of sending, where a
  duplicate is preferred to a lost message.
- **Catching up.** After downtime it resumes from where it stopped. Mail that shows up
  late, such as once its sender is let out of the Screener, is still forwarded while it
  is younger than `lookback`.
- **Failures.** If HEY is unreachable the poll is retried at the next interval. A
  forward HEY refuses is logged and retried every 15 minutes until it ages past
  `lookback`.

## Running under systemd

[`hey-forward.service`](hey-forward.service) runs it as its own user and finds the
config in `/etc/hey-forward` and its state in `/var/lib/hey-forward` without flags:

```sh
sudo install target/release/hey-forward /usr/local/bin/
sudo useradd --system --create-home --home-dir /var/lib/hey-forward \
     --shell /usr/sbin/nologin hey-forward
sudo install -Dm644 config.example.toml /etc/hey-forward/config.toml   # then edit
sudo install -m644 hey-forward.service /etc/systemd/system/

sudo -u hey-forward hey-forward --state-dir /var/lib/hey-forward login
sudo systemctl enable --now hey-forward
journalctl -fu hey-forward
```

## Files and flags

| | Default | Override |
| --- | --- | --- |
| Rules | `~/.config/hey-forward/config.toml` | `--config`, `HEY_FORWARD_CONFIG` |
| Credentials and progress | `~/.local/state/hey-forward/` | `--state-dir`, `HEY_FORWARD_STATE_DIR` |

The state directory holds `credentials.json` (HEY OAuth tokens, which grant access to
the whole mailbox, written owner-only) and `state.json` (what has been handled). Deleting
`state.json` makes the next `run` a first run.

`-v` logs every message looked at together with the addresses it was sent to, which
shows how an alias appears on the wire. `HEY_FORWARD_LOG` takes a full
[`tracing` filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html).
