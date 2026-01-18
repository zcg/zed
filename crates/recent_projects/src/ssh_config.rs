use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshConfigEntry {
    pub host: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
}

pub fn parse_ssh_config_hosts(config: &str) -> BTreeSet<String> {
    let mut hosts = BTreeSet::new();
    let mut needs_another_line = false;
    for line in config.lines() {
        let mut line = line.trim_start();
        if let Some((before, _)) = line.split_once('#') {
            line = before.trim_end();
        }
        if let Some(line) = line.strip_prefix("Host") {
            match line.chars().next() {
                Some('\\') => {
                    needs_another_line = true;
                }
                Some('\n' | '\r') => {
                    needs_another_line = false;
                }
                Some(c) if c.is_whitespace() => {
                    parse_hosts_from(line, &mut hosts);
                }
                Some(_) | None => {
                    needs_another_line = false;
                }
            };

            if needs_another_line {
                parse_hosts_from(line, &mut hosts);
                needs_another_line = line.trim_end().ends_with('\\');
            } else {
                needs_another_line = false;
            }
        } else if needs_another_line {
            needs_another_line = line.trim_end().ends_with('\\');
            parse_hosts_from(line, &mut hosts);
        } else {
            needs_another_line = false;
        }
    }

    hosts
}

pub fn parse_ssh_config_entries(config: &str) -> HashMap<String, SshConfigEntry> {
    let mut entries: HashMap<String, SshConfigEntry> = HashMap::new();
    let mut current_hosts: Vec<String> = Vec::new();
    let mut pending_hosts: Vec<String> = Vec::new();
    let mut needs_another_line = false;

    for line in config.lines() {
        let mut line = line.trim_start();
        if let Some((before, _)) = line.split_once('#') {
            line = before.trim_end();
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if needs_another_line {
            pending_hosts.extend(parse_hosts_from_line(line));
            needs_another_line = line.trim_end().ends_with('\\');
            if !needs_another_line {
                current_hosts = pending_hosts.drain(..).collect();
            }
            continue;
        }

        if let Some(rest) = strip_prefix_keyword(line, "Host") {
            pending_hosts.extend(parse_hosts_from_line(rest));
            needs_another_line = line.trim_end().ends_with('\\');
            if !needs_another_line {
                current_hosts = pending_hosts.drain(..).collect();
            }
            continue;
        }

        if strip_prefix_keyword(line, "Match").is_some() {
            current_hosts.clear();
            continue;
        }

        if current_hosts.is_empty() {
            continue;
        }

        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        let value = parts.collect::<Vec<_>>().join(" ");
        if value.is_empty() {
            continue;
        }

        for host in &current_hosts {
            let entry = entries
                .entry(host.to_string())
                .or_insert_with(|| SshConfigEntry {
                    host: host.to_string(),
                    ..Default::default()
                });
            if key.eq_ignore_ascii_case("user") {
                entry.user = Some(value.clone());
            } else if key.eq_ignore_ascii_case("port") {
                entry.port = value.parse::<u16>().ok();
            } else if key.eq_ignore_ascii_case("hostname") {
                entry.hostname = Some(value.clone());
            }
        }
    }

    entries
}

fn parse_hosts_from(line: &str, hosts: &mut BTreeSet<String>) {
    hosts.extend(
        line.split_whitespace()
            .filter(|field| !field.starts_with("!"))
            .filter(|field| !field.contains("*"))
            .filter(|field| !field.is_empty())
            .map(|field| field.to_owned()),
    );
}

fn parse_hosts_from_line(line: &str) -> Vec<String> {
    let line = line.split_once('#').map(|(before, _)| before).unwrap_or(line);
    line.split_whitespace()
        .filter(|field| !field.starts_with('!'))
        .filter(|field| !field.contains('*'))
        .filter(|field| !field.is_empty())
        .map(|field| field.trim_end_matches('\\').to_owned())
        .filter(|field| !field.is_empty())
        .collect()
}

fn strip_prefix_keyword<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    if value.len() < prefix.len() {
        return None;
    }
    let (head, tail) = value.split_at(prefix.len());
    if head.eq_ignore_ascii_case(prefix) {
        if tail.is_empty()
            || tail
                .chars()
                .next()
                .is_some_and(|c| c.is_whitespace())
        {
            Some(tail)
        } else {
            None
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_thank_you_bjorn3() {
        let hosts = "
            Host *
              AddKeysToAgent yes
              UseKeychain yes
              IdentityFile ~/.ssh/id_ed25519

            Host whatever.*
            User another

            Host !not_this
            User not_me

            Host something
        HostName whatever.tld

        Host linux bsd host3
          User bjorn

        Host rpi
          user rpi
          hostname rpi.local

        Host \
               somehost \
        anotherhost
        Hostname 192.168.3.3";

        let expected_hosts = BTreeSet::from_iter([
            "something".to_owned(),
            "linux".to_owned(),
            "host3".to_owned(),
            "bsd".to_owned(),
            "rpi".to_owned(),
            "somehost".to_owned(),
            "anotherhost".to_owned(),
        ]);

        assert_eq!(expected_hosts, parse_ssh_config_hosts(hosts));
    }
}
