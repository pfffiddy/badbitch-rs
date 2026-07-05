//! Target classification — ports `_classify_target` (badbitch2.py:1269) and its helpers
//! `_is_ip` (224), `_looks_domain` (232), and the DOB/address regexes (1262).

use std::sync::LazyLock;

use regex::Regex;

static RE_DOB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(\d{4}-\d{1,2}-\d{1,2}|\d{1,2}[-/]\d{1,2}[-/]\d{2,4})\b").unwrap()
});
static RE_ADDR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b\d{1,6}\s+(?:[NSEW]\.?\s+)?[A-Za-z0-9.\- ]+?\s(?:st|street|ave|avenue|rd|road|dr|drive|ln|lane|blvd|boulevard|ct|court|cir|circle|way|pl|place|hwy|pkwy|ter|terrace|trl|trail)\b\.?",
    )
    .unwrap()
});
static RE_ADDR_LOOSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{2,6}\s+(?:[NSEW]\b\.?\s+)?[A-Z][A-Za-z]+\b").unwrap());
static RE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Z][a-zA-Z'\x{2019}\-]+\b").unwrap());
static RE_EMAIL_FULL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^@\s]+@[^@\s]+\.\w+$").unwrap());
static RE_DOMAIN_FULL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9.-]+\.[a-z]{2,}$").unwrap());
static RE_USERNAME_FULL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.\-]{3,30}$").unwrap());

#[derive(Debug, Default, Clone)]
pub struct Classified {
    pub raw: String,
    pub kind: String,
    pub email: String,
    pub ip: String,
    pub domain: String,
    pub username: String,
    pub dob: String,
    pub address: String,
    pub name: String,
}

pub fn is_ip(s: &str) -> bool {
    s.trim().parse::<std::net::IpAddr>().is_ok()
}

pub fn looks_domain(s: &str) -> bool {
    let s = s.trim().to_lowercase();
    RE_DOMAIN_FULL.is_match(&s) && !is_ip(&s)
}

fn norm_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `_classify_target` (badbitch2.py:1269): best-effort split of a raw target line into typed
/// vectors. Person-first: a human name makes an address a mere attribute, not a new subject.
pub fn classify_target(target: &str) -> Classified {
    let t = norm_ws(target);
    let mut info = Classified {
        raw: t.clone(),
        kind: "unknown".to_string(),
        ..Default::default()
    };

    if RE_EMAIL_FULL.is_match(&t) {
        info.kind = "email".to_string();
        info.email = t;
        return info;
    }
    if is_ip(&t) {
        info.kind = "ip".to_string();
        info.ip = t;
        return info;
    }
    if looks_domain(&t) {
        info.kind = "domain".to_string();
        info.domain = t;
        return info;
    }

    if let Some(m) = RE_DOB.captures(&t).and_then(|c| c.get(1)) {
        info.dob = m.as_str().to_string();
    }

    // Search a DOB-stripped copy so a birth-year can't masquerade as a house number.
    let work = if info.dob.is_empty() {
        t.clone()
    } else {
        t.replace(&info.dob, " ")
    };

    let addr_match = RE_ADDR.find(&work).or_else(|| RE_ADDR_LOOSE.find(&work));
    let addr_start = match &addr_match {
        Some(m) => {
            info.address = norm_ws(m.as_str());
            m.start()
        }
        None => work.len(),
    };

    // Names precede the address: read only the text BEFORE the address.
    let names: Vec<String> = RE_NAME
        .find_iter(&work[..addr_start])
        .map(|m| m.as_str().to_string())
        .collect();
    if names.len() >= 2 {
        info.name = names.into_iter().take(4).collect::<Vec<_>>().join(" ");
        info.kind = "person".to_string();
    } else if !info.address.is_empty() {
        info.kind = "address".to_string();
    } else if RE_USERNAME_FULL.is_match(&t) {
        info.kind = "username".to_string();
        info.username = t;
    }

    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_ip_domain() {
        assert_eq!(classify_target("bob@example.com").kind, "email");
        assert_eq!(classify_target("8.8.8.8").kind, "ip");
        assert_eq!(classify_target("example.com").kind, "domain");
    }

    #[test]
    fn person_with_dob_and_address() {
        let c = classify_target("Jane Q Public 8-13-92 16303 N Aster");
        assert_eq!(c.kind, "person");
        assert!(c.name.contains("Jane"));
        assert!(c.dob.contains("92"));
        assert!(!c.address.is_empty());
    }

    #[test]
    fn bare_address_is_address_not_person() {
        let c = classify_target("4207 Valley View Rd");
        assert_eq!(c.kind, "address");
    }

    #[test]
    fn username_fallback() {
        assert_eq!(classify_target("n1ght_0wl").kind, "username");
    }

    #[test]
    fn helpers() {
        assert!(is_ip("1.2.3.4"));
        assert!(!is_ip("1.2.3"));
        assert!(looks_domain("a.co"));
        assert!(!looks_domain("1.2.3.4"));
    }
}
