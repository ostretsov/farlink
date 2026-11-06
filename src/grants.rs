//! The relay server's saved state: admin password, enrolled machines and
//! temporary access links ("grants"). Pure logic, no networking.

use crate::proto::MonitorInfo;
use crate::util::{constant_time_eq, random_token, sha256_hex};
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Admin sign-ins last 30 days.
pub const ADMIN_SESSION_SECS: u64 = 30 * 86_400;
/// Longest a temporary link may live: 30 days.
pub const MAX_GRANT_MINUTES: u64 = 30 * 24 * 60;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    /// Name shown in lists (admin can rename).
    pub name: String,
    pub hostname: String,
    pub os: String,
    pub version: String,
    pub target: String,
    /// SHA-256 of the machine secret.
    pub secret_hash: String,
    pub monitors: Vec<MonitorInfo>,
    pub first_seen: u64,
    pub last_seen: u64,
    /// Person (user id) this computer belongs to; only they and the admin see it.
    #[serde(default)]
    pub owner: Option<String>,
}

/// A person with access to their own computers only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
    /// Sign-in name (lowercase).
    pub login: String,
    pub password_hash: String,
    /// Personal install link token: computers installed with it belong to this person.
    pub enroll_token: String,
    pub created: u64,
}

/// Who is signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    Admin,
    /// User id.
    User(String),
}

/// A computer's display name: 1-80 characters after trimming, no control characters.
pub fn valid_machine_name(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty() && n.chars().count() <= 80 && !n.chars().any(char::is_control)
}

/// Check a sign-in name typed by the admin.
pub fn valid_login(l: &str) -> bool {
    !l.is_empty()
        && l.len() <= 40
        && l != "admin"
        && l.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub token: String,
    pub machine_id: String,
    pub label: String,
    pub created: u64,
    pub expires: u64,
    pub view_only: bool,
    pub revoked: bool,
    #[serde(default)]
    pub uses: u32,
}

impl Grant {
    pub fn is_active(&self, now: u64) -> bool {
        !self.revoked && now < self.expires
    }

    pub fn status(&self, now: u64) -> &'static str {
        if self.revoked {
            "withdrawn"
        } else if now >= self.expires {
            "expired"
        } else {
            "active"
        }
    }
}

/// What a host tells the server about itself when registering.
#[derive(Debug, Clone)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub version: String,
    pub target: String,
    pub monitors: Vec<MonitorInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Store {
    pub admin_hash: String,
    /// Token inside install links. Rotating it invalidates old links.
    pub enroll_token: String,
    #[serde(default)]
    pub machines: BTreeMap<String, Machine>,
    /// token -> grant
    #[serde(default)]
    pub grants: BTreeMap<String, Grant>,
    /// sha256(admin session token) -> expiry
    #[serde(default)]
    pub admin_sessions: BTreeMap<String, u64>,
    /// user id -> person
    #[serde(default)]
    pub users: BTreeMap<String, User>,
    /// sha256(user session token) -> (user id, expiry)
    #[serde(default)]
    pub user_sessions: BTreeMap<String, (String, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    BadEnrollToken,
    UnknownMachine,
    BadSecret,
    InvalidRequest(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::BadEnrollToken => write!(f, "install link is not valid any more"),
            StoreError::UnknownMachine => write!(f, "machine was removed from the server"),
            StoreError::BadSecret => write!(f, "machine key does not match"),
            StoreError::InvalidRequest(s) => write!(f, "{s}"),
        }
    }
}

pub fn hash_password(pw: &str) -> String {
    Argon2::default().hash_password(pw.as_bytes()).map(|h| h.to_string()).unwrap_or_default()
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default().verify_password(pw.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

impl Store {
    pub fn new(admin_password: &str) -> Store {
        Store { admin_hash: hash_password(admin_password), enroll_token: random_token(16), ..Default::default() }
    }

    pub fn set_password(&mut self, pw: &str) {
        self.admin_hash = hash_password(pw);
        self.admin_sessions.clear();
    }

    /// Check the password and open an admin session. Returns the session token.
    pub fn login(&mut self, pw: &str, now: u64) -> Option<String> {
        if !verify_password(pw, &self.admin_hash) {
            return None;
        }
        let token = random_token(24);
        self.admin_sessions.insert(sha256_hex(token.as_bytes()), now + ADMIN_SESSION_SECS);
        Some(token)
    }

    pub fn check_admin(&self, token: &str, now: u64) -> bool {
        !token.is_empty() && self.admin_sessions.get(&sha256_hex(token.as_bytes())).is_some_and(|&exp| now < exp)
    }

    pub fn logout(&mut self, token: &str) {
        let h = sha256_hex(token.as_bytes());
        self.admin_sessions.remove(&h);
        self.user_sessions.remove(&h);
    }

    pub fn rotate_enroll_token(&mut self) -> String {
        self.enroll_token = random_token(16);
        self.enroll_token.clone()
    }

    /// Register a host. New hosts need the enrol token; known hosts their
    /// secret. Returns (machine id, secret to keep, display name).
    pub fn register(
        &mut self,
        enroll_token: Option<&str>,
        machine_id: Option<&str>,
        secret: Option<&str>,
        info: HostInfo,
        now: u64,
    ) -> Result<(String, String, String), StoreError> {
        if let (Some(id), Some(secret)) = (machine_id, secret) {
            let m = self.machines.get_mut(id).ok_or(StoreError::UnknownMachine)?;
            if !constant_time_eq(&m.secret_hash, &sha256_hex(secret.as_bytes())) {
                return Err(StoreError::BadSecret);
            }
            m.hostname = info.hostname;
            m.os = info.os;
            m.version = info.version;
            m.target = info.target;
            m.monitors = info.monitors;
            m.last_seen = now;
            return Ok((m.id.clone(), secret.to_string(), m.name.clone()));
        }
        let token = enroll_token.ok_or(StoreError::BadEnrollToken)?;
        let owner = self.enroll_owner(token).ok_or(StoreError::BadEnrollToken)?;
        let id = random_token(8);
        let secret = random_token(32);
        let name = self.unique_name(&info.hostname);
        self.machines.insert(
            id.clone(),
            Machine {
                id: id.clone(),
                name: name.clone(),
                hostname: info.hostname,
                os: info.os,
                version: info.version,
                target: info.target,
                secret_hash: sha256_hex(secret.as_bytes()),
                monitors: info.monitors,
                first_seen: now,
                last_seen: now,
                owner,
            },
        );
        Ok((id, secret, name))
    }

    /// Which install link is this? `Some(None)` = the general link,
    /// `Some(Some(user))` = a person's own link, `None` = not valid.
    pub fn enroll_owner(&self, token: &str) -> Option<Option<String>> {
        if token.is_empty() {
            return None;
        }
        if !self.enroll_token.is_empty() && constant_time_eq(token, &self.enroll_token) {
            return Some(None);
        }
        self.users.values().find(|u| constant_time_eq(token, &u.enroll_token)).map(|u| Some(u.id.clone()))
    }

    // -------------------------------------------------------------- people

    pub fn create_user(&mut self, name: &str, login: &str, password: &str, now: u64) -> Result<User, StoreError> {
        let name = name.trim();
        let login = login.trim().to_lowercase();
        if name.is_empty() || name.len() > 80 {
            return Err(StoreError::InvalidRequest("name must be 1-80 characters".into()));
        }
        if !valid_login(&login) {
            return Err(StoreError::InvalidRequest("sign-in name: lowercase letters, digits, . _ - (not 'admin')".into()));
        }
        if self.users.values().any(|u| u.login == login) {
            return Err(StoreError::InvalidRequest(format!("sign-in name '{login}' is already used")));
        }
        if password.chars().count() < 8 {
            return Err(StoreError::InvalidRequest("password needs at least 8 characters".into()));
        }
        let u = User {
            id: random_token(8),
            name: name.to_string(),
            login,
            password_hash: hash_password(password),
            enroll_token: random_token(16),
            created: now,
        };
        self.users.insert(u.id.clone(), u.clone());
        Ok(u)
    }

    pub fn set_user_password(&mut self, id: &str, password: &str) -> Result<(), StoreError> {
        if password.chars().count() < 8 {
            return Err(StoreError::InvalidRequest("password needs at least 8 characters".into()));
        }
        let u = self.users.get_mut(id).ok_or(StoreError::InvalidRequest("no such person".into()))?;
        u.password_hash = hash_password(password);
        self.user_sessions.retain(|_, (uid, _)| uid != id);
        Ok(())
    }

    /// Remove a person: their computers stay (admin only), their sign-ins end.
    pub fn delete_user(&mut self, id: &str) -> Result<(), StoreError> {
        self.users.remove(id).ok_or(StoreError::InvalidRequest("no such person".into()))?;
        for m in self.machines.values_mut().filter(|m| m.owner.as_deref() == Some(id)) {
            m.owner = None;
        }
        self.user_sessions.retain(|_, (uid, _)| uid != id);
        Ok(())
    }

    /// Give a computer to a person (or back to admin-only with `None`).
    pub fn assign(&mut self, machine_id: &str, owner: Option<&str>) -> Result<(), StoreError> {
        if let Some(uid) = owner
            && !self.users.contains_key(uid)
        {
            return Err(StoreError::InvalidRequest("no such person".into()));
        }
        self.machines.get_mut(machine_id).ok_or(StoreError::UnknownMachine)?.owner = owner.map(str::to_string);
        Ok(())
    }

    /// Sign in as admin (empty name or "admin") or as a person.
    pub fn login_as(&mut self, login: &str, pw: &str, now: u64) -> Option<(String, Who, String)> {
        let login = login.trim().to_lowercase();
        if login.is_empty() || login == "admin" {
            return self.login(pw, now).map(|t| (t, Who::Admin, "Administrator".to_string()));
        }
        let u = self.users.values().find(|u| u.login == login)?;
        if !verify_password(pw, &u.password_hash) {
            return None;
        }
        let (id, name) = (u.id.clone(), u.name.clone());
        let token = random_token(24);
        self.user_sessions.insert(sha256_hex(token.as_bytes()), (id.clone(), now + ADMIN_SESSION_SECS));
        Some((token, Who::User(id), name))
    }

    /// Who does this session token belong to?
    pub fn who(&self, token: &str, now: u64) -> Option<Who> {
        if token.is_empty() {
            return None;
        }
        if self.check_admin(token, now) {
            return Some(Who::Admin);
        }
        let (uid, exp) = self.user_sessions.get(&sha256_hex(token.as_bytes()))?;
        (now < *exp && self.users.contains_key(uid)).then(|| Who::User(uid.clone()))
    }

    /// May `who` see and control this computer?
    pub fn can_access(&self, who: &Who, machine_id: &str) -> bool {
        match who {
            Who::Admin => self.machines.contains_key(machine_id),
            Who::User(uid) => self.machines.get(machine_id).is_some_and(|m| m.owner.as_deref() == Some(uid.as_str())),
        }
    }

    pub fn display_name(&self, who: &Who) -> String {
        match who {
            Who::Admin => "Administrator".into(),
            Who::User(uid) => self.users.get(uid).map(|u| u.name.clone()).unwrap_or_default(),
        }
    }

    fn unique_name(&self, base: &str) -> String {
        let base = if base.trim().is_empty() { "computer" } else { base.trim() };
        let taken = |n: &str| self.machines.values().any(|m| m.name == n);
        if !taken(base) {
            return base.to_string();
        }
        (2..).map(|i| format!("{base} ({i})")).find(|n| !taken(n)).unwrap()
    }

    pub fn rename_machine(&mut self, id: &str, name: &str) -> Result<(), StoreError> {
        let name = name.trim();
        if !valid_machine_name(name) {
            return Err(StoreError::InvalidRequest("name must be 1-80 characters".into()));
        }
        self.machines.get_mut(id).ok_or(StoreError::UnknownMachine)?.name = name.to_string();
        Ok(())
    }

    /// Remove a machine and withdraw all its links. Returns withdrawn tokens.
    pub fn remove_machine(&mut self, id: &str) -> Result<Vec<String>, StoreError> {
        self.machines.remove(id).ok_or(StoreError::UnknownMachine)?;
        let mut withdrawn = Vec::new();
        for g in self.grants.values_mut().filter(|g| g.machine_id == id) {
            g.revoked = true;
            withdrawn.push(g.token.clone());
        }
        Ok(withdrawn)
    }

    pub fn create_grant(
        &mut self,
        machine_id: &str,
        minutes: u64,
        view_only: bool,
        label: &str,
        now: u64,
    ) -> Result<Grant, StoreError> {
        if !self.machines.contains_key(machine_id) {
            return Err(StoreError::UnknownMachine);
        }
        if minutes == 0 || minutes > MAX_GRANT_MINUTES {
            return Err(StoreError::InvalidRequest("duration must be 1 minute to 30 days".into()));
        }
        let g = Grant {
            token: random_token(20),
            machine_id: machine_id.to_string(),
            label: label.trim().chars().take(80).collect(),
            created: now,
            expires: now + minutes * 60,
            view_only,
            revoked: false,
            uses: 0,
        };
        self.grants.insert(g.token.clone(), g.clone());
        Ok(g)
    }

    /// Withdraw a link. Returns true if it existed and was active.
    pub fn revoke(&mut self, token: &str) -> bool {
        match self.grants.get_mut(token) {
            Some(g) if !g.revoked => {
                g.revoked = true;
                true
            }
            _ => false,
        }
    }

    pub fn active_grant(&self, token: &str, now: u64) -> Option<&Grant> {
        self.grants.get(token).filter(|g| g.is_active(now))
    }

    /// Delete links that ended more than a week ago, and old admin sessions.
    pub fn prune(&mut self, now: u64) -> usize {
        let before = self.grants.len();
        self.grants.retain(|_, g| {
            let ended = if g.revoked { g.created.max(g.expires.min(now)) } else { g.expires };
            now < ended + 7 * 86_400
        });
        self.admin_sessions.retain(|_, exp| now < *exp);
        self.user_sessions.retain(|_, (_, exp)| now < *exp);
        before - self.grants.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str) -> HostInfo {
        HostInfo {
            hostname: name.into(),
            os: "Windows".into(),
            version: "0.1.0".into(),
            target: "x86_64-pc-windows-gnu".into(),
            monitors: vec![],
        }
    }

    #[test]
    fn password_login_and_sessions() {
        let mut s = Store::new("correct horse");
        assert!(s.login("wrong", 0).is_none());
        let t = s.login("correct horse", 100).unwrap();
        assert!(s.check_admin(&t, 200));
        assert!(!s.check_admin(&t, 100 + ADMIN_SESSION_SECS));
        assert!(!s.check_admin("", 200));
        s.logout(&t);
        assert!(!s.check_admin(&t, 200));
        let t2 = s.login("correct horse", 0).unwrap();
        s.set_password("new");
        assert!(!s.check_admin(&t2, 1), "changing password signs everyone out");
        assert!(s.login("new", 0).is_some());
    }

    #[test]
    fn enrolment_then_reconnect_with_secret() {
        let mut s = Store::new("pw");
        let tok = s.enroll_token.clone();
        assert_eq!(s.register(Some("nope"), None, None, info("pc"), 0), Err(StoreError::BadEnrollToken));
        let (id, secret, name) = s.register(Some(&tok), None, None, info("pc"), 10).unwrap();
        assert_eq!(name, "pc");
        let (id2, _, _) = s.register(None, Some(&id), Some(&secret), info("pc-renamed"), 20).unwrap();
        assert_eq!(id, id2);
        assert_eq!(s.machines[&id].hostname, "pc-renamed");
        assert_eq!(s.machines[&id].last_seen, 20);
        assert_eq!(s.register(None, Some(&id), Some("bad"), info("pc"), 30), Err(StoreError::BadSecret));
        assert_eq!(s.register(None, Some("ghost"), Some(&secret), info("pc"), 30), Err(StoreError::UnknownMachine));
        // Second machine with the same hostname gets a distinct name.
        let (_, _, n2) = s.register(Some(&tok), None, None, info("pc"), 40).unwrap();
        assert_eq!(n2, "pc (2)");
        // Rotated token stops old install links.
        s.rotate_enroll_token();
        assert_eq!(s.register(Some(&tok), None, None, info("x"), 50), Err(StoreError::BadEnrollToken));
    }

    #[test]
    fn grants_expire_and_can_be_withdrawn() {
        let mut s = Store::new("pw");
        let tok = s.enroll_token.clone();
        let (id, _, _) = s.register(Some(&tok), None, None, info("srv"), 0).unwrap();
        assert_eq!(s.create_grant("ghost", 10, true, "", 0), Err(StoreError::UnknownMachine));
        assert!(s.create_grant(&id, 0, true, "", 0).is_err());
        let g = s.create_grant(&id, 60, true, "  Support for Ana  ", 1000).unwrap();
        assert_eq!(g.label, "Support for Ana");
        assert_eq!(g.expires, 1000 + 3600);
        assert!(s.active_grant(&g.token, 1000).is_some());
        assert!(s.active_grant(&g.token, 1000 + 3600).is_none(), "expired");
        assert_eq!(s.grants[&g.token].status(5000), "expired");

        let g2 = s.create_grant(&id, 60, false, "", 1000).unwrap();
        assert!(s.revoke(&g2.token));
        assert!(!s.revoke(&g2.token), "already withdrawn");
        assert!(s.active_grant(&g2.token, 1001).is_none());
        assert_eq!(s.grants[&g2.token].status(1001), "withdrawn");
    }

    #[test]
    fn removing_machine_withdraws_its_links() {
        let mut s = Store::new("pw");
        let tok = s.enroll_token.clone();
        let (id, _, _) = s.register(Some(&tok), None, None, info("a"), 0).unwrap();
        let g = s.create_grant(&id, 60, false, "", 0).unwrap();
        assert_eq!(s.remove_machine(&id).unwrap(), vec![g.token.clone()]);
        assert!(s.active_grant(&g.token, 1).is_none());
        assert_eq!(s.remove_machine(&id), Err(StoreError::UnknownMachine));
    }

    #[test]
    fn prune_drops_old_links() {
        let mut s = Store::new("pw");
        let tok = s.enroll_token.clone();
        let (id, _, _) = s.register(Some(&tok), None, None, info("a"), 0).unwrap();
        let old = s.create_grant(&id, 1, false, "", 0).unwrap();
        let fresh = s.create_grant(&id, 60 * 24 * 20, false, "", 0).unwrap();
        assert_eq!(s.prune(8 * 86_400), 1);
        assert!(!s.grants.contains_key(&old.token));
        assert!(s.grants.contains_key(&fresh.token));
    }

    #[test]
    fn personal_install_link_gives_computer_to_that_person_only() {
        let mut s = Store::new("admin-pw");
        let anna = s.create_user("Anna", "Anna", "anna-secret", 0).unwrap();
        assert_eq!(anna.login, "anna");
        assert!(s.create_user("Other Anna", "anna", "12345678", 0).is_err(), "sign-in names are unique");
        assert!(s.create_user("X", "admin", "12345678", 0).is_err());
        assert!(s.create_user("X", "x", "short", 0).is_err());

        let general = s.enroll_token.clone();
        let (pc_anna, _, _) = s.register(Some(&anna.enroll_token), None, None, info("ANNA-PC"), 0).unwrap();
        let (pc_office, _, _) = s.register(Some(&general), None, None, info("OFFICE"), 0).unwrap();
        assert_eq!(s.machines[&pc_anna].owner.as_deref(), Some(anna.id.as_str()));
        assert_eq!(s.machines[&pc_office].owner, None);

        assert!(s.login_as("anna", "wrong", 0).is_none());
        let (tok, who, name) = s.login_as(" Anna ", "anna-secret", 0).unwrap();
        assert_eq!(name, "Anna");
        assert_eq!(s.who(&tok, 1), Some(who.clone()));
        assert!(!s.check_admin(&tok, 1), "a person is never an admin");
        assert!(s.can_access(&who, &pc_anna));
        assert!(!s.can_access(&who, &pc_office));
        assert!(s.can_access(&Who::Admin, &pc_office));

        // Admin moves the office PC to Anna, then removes Anna.
        s.assign(&pc_office, Some(&anna.id)).unwrap();
        assert!(s.can_access(&who, &pc_office));
        s.delete_user(&anna.id).unwrap();
        assert_eq!(s.who(&tok, 1), None, "her sign-in ends");
        assert_eq!(s.machines[&pc_anna].owner, None, "computers stay, admin-only");
        assert!(s.enroll_owner(&anna.enroll_token).is_none(), "her install link stops working");

        let (atok, awho, _) = s.login_as("", "admin-pw", 0).unwrap();
        assert_eq!(awho, Who::Admin);
        assert_eq!(s.who(&atok, 1), Some(Who::Admin));
    }

    #[test]
    fn store_serializes() {
        let s = Store::new("pw");
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("\"pw\""), "password never stored in clear");
        let back: Store = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
