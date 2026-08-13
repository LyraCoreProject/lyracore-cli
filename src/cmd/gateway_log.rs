//! Structured evidence from the latest gateway start in a possibly append-only log.

pub(crate) const CONNECTED_MARKER: &str = "coordinator connected to shard";

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct GatewayEvidence {
    pub saw_start: bool,
    pub configured: Option<Vec<String>>,
    pub connected: Vec<String>,
    pub realm_core: Option<String>,
    pub logon_listener: Option<String>,
    pub world_listener: Option<String>,
    pub startup_errors: usize,
    pub realm_address_warning: bool,
    pub metrics_warning: bool,
}

impl GatewayEvidence {
    pub fn parse(log: &str) -> Self {
        let start = log.rfind("gateway starting:");
        let latest = start.map_or(log, |at| &log[at..]);
        let mut evidence = Self {
            saw_start: start.is_some(),
            ..Self::default()
        };

        for line in latest.lines() {
            if let Some((_, tail)) = line.split_once(CONNECTED_MARKER) {
                if let Some(name) = plausible_database(tail.split_whitespace().next()) {
                    push_unique(&mut evidence.connected, name);
                }
            }
            if let Some((_, tail)) = line.split_once("shard map active:") {
                evidence.configured = parse_configured(tail);
            }
            if let Some((_, tail)) = line.split_once("realm-core active:") {
                evidence.realm_core = tail
                    .split_once("live on ")
                    .and_then(|(_, name)| plausible_database(name.split_whitespace().next()))
                    .map(str::to_string);
            }
            if let Some((_, tail)) = line.split_once("logon listening on ") {
                evidence.logon_listener = tail.split_whitespace().next().map(str::to_string);
            }
            if let Some((_, tail)) = line.split_once("world listening on ") {
                evidence.world_listener = tail.split_whitespace().next().map(str::to_string);
            }
            if line.contains(" ERROR ") || line.contains(" panicked at ") {
                evidence.startup_errors += 1;
            }
            evidence.realm_address_warning |= line.contains("realm advertises ");
            evidence.metrics_warning |= line.contains("LYRACORE_METRICS_DB_IDS is unset")
                || line.contains("occupancy=unmeasured");
        }
        evidence
    }
}

pub(crate) fn connected_shards(log: &str) -> Vec<String> {
    GatewayEvidence::parse(log).connected
}

fn plausible_database(candidate: Option<&str>) -> Option<&str> {
    candidate.filter(|name| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

fn push_unique(names: &mut Vec<String>, name: &str) {
    if !names.iter().any(|known| known == name) {
        names.push(name.to_string());
    }
}

fn parse_configured(line: &str) -> Option<Vec<String>> {
    let (_, list) = line.split_once("databases [")?;
    let (list, _) = list.split_once(']')?;
    let mut databases = Vec::new();
    for raw in list.split(',') {
        let name = raw.trim().trim_matches('"');
        let name = plausible_database(Some(name))?;
        push_unique(&mut databases, name);
    }
    Some(databases)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_start_wins_and_every_signal_is_structured() {
        let evidence = GatewayEvidence::parse(
            "gateway starting: stale\n\
             ERROR stale boot\n\
             coordinator connected to shard old\n\
             gateway starting: current\n\
             coordinator connected to shard lyracore\n\
             coordinator connected to shard lyracore-realm\n\
             coordinator connected to shard lyracore\n\
             realm-core active: accounts live on lyracore-realm\n\
             shard map active: 2 databases [\"lyracore\", \"lyracore-realm\"]\n\
             logon listening on 0.0.0.0:3724\n\
             world listening on 0.0.0.0:8085\n\
             WARN realm advertises 127.0.0.1:8085 but the world listener is bound\n\
             WARN LYRACORE_METRICS_DB_IDS is unset\n",
        );

        assert!(evidence.saw_start);
        assert_eq!(evidence.connected, vec!["lyracore", "lyracore-realm"]);
        assert_eq!(
            evidence.configured,
            Some(vec!["lyracore".into(), "lyracore-realm".into()])
        );
        assert_eq!(evidence.realm_core.as_deref(), Some("lyracore-realm"));
        assert_eq!(evidence.logon_listener.as_deref(), Some("0.0.0.0:3724"));
        assert_eq!(evidence.world_listener.as_deref(), Some("0.0.0.0:8085"));
        assert_eq!(evidence.startup_errors, 0);
        assert!(evidence.realm_address_warning);
        assert!(evidence.metrics_warning);
    }

    #[test]
    fn quoted_advice_is_not_a_connection() {
        let evidence = GatewayEvidence::parse(
            "gateway starting: current\n\
             WARN expected `coordinator connected to shard` for each database\n",
        );
        assert!(evidence.connected.is_empty());
    }
}
