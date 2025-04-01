#[derive(Debug, Clone, Default)]
pub struct Acl {
    matches: Vec<HostMatch>,
}

impl Acl {
    pub fn new(matches: Vec<HostMatch>) -> Self {
        Self { matches }
    }

    pub fn check_host(&self, host: &str) -> bool {
        let host_lower = host.to_ascii_lowercase();
        let remain_lower = host_lower.split_once('.').map(|(_, remain)| remain);
        self.matches
            .iter()
            .any(|m| m.matches(&host_lower, remain_lower))
    }
}

#[derive(Debug, Clone)]
pub enum HostMatch {
    AllAllow,
    HeaderFuzzy(String),
    Exact(String),
}

impl HostMatch {
    fn from_pattern(host: String) -> Self {
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

pub fn parse_host_matches(hosts: Vec<String>) -> Vec<HostMatch> {
    hosts
        .into_iter()
        .map(HostMatch::from_pattern) // Use the associated function
        .collect()
}
