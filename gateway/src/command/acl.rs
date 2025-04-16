#[derive(Debug, Clone, Default)]
pub struct Acl {
    allow: Vec<HostMatch>,
    deny: Vec<HostMatch>,
}

impl Acl {
    pub fn new(allow: Vec<HostMatch>, deny: Vec<HostMatch>) -> Self {
        Self { allow, deny }
    }

    pub fn check(&self, host: &str) -> bool {
        if self.allow.is_empty() {
            return false;
        }

        if !check_host(&self.allow, host) {
            return false;
        }

        if self.deny.is_empty() {
            return true;
        }

        if check_host(&self.deny, host) {
            return false;
        }

        true
    }
}

#[derive(Debug, Clone)]
pub enum HostMatch {
    AllAllow,
    HeaderFuzzy(String),
    Exact(String),
}

impl HostMatch {
    fn from_pattern(host: &str) -> Self {
        if host == "*" {
            return HostMatch::AllAllow;
        }
        let (header, remain) = host
            .split_once('.')
            .expect("Host pattern string must contain a '.'");
        if header == "*" {
            HostMatch::HeaderFuzzy(remain.to_ascii_lowercase())
        } else {
            HostMatch::Exact(host.to_ascii_lowercase())
        }
    }

    fn matches(&self, check_host_lower: &str, check_remain_lower: Option<&str>) -> bool {
        match self {
            HostMatch::AllAllow => true,
            HostMatch::HeaderFuzzy(remain) => {
                // Only match if check_host had a '.' and the remaining part matches
                check_remain_lower.is_some_and(|cr| remain == cr)
            }
            HostMatch::Exact(host) => {
                // Direct comparison with the full lowercased host
                host == check_host_lower
            }
        }
    }
}

pub fn parse_host_matches(hosts: &[String]) -> Vec<HostMatch> {
    hosts
        .iter()
        .map(|host| HostMatch::from_pattern(host)) // Use the associated function
        .collect()
}

fn check_host(matches: &[HostMatch], host: &str) -> bool {
    let host_lower = host.to_ascii_lowercase();
    let remain_lower = host_lower.split_once('.').map(|(_, remain)| remain);
    matches.iter().any(|m| m.matches(&host_lower, remain_lower))
}
