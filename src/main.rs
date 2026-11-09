use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use chrono::{Datelike, Local, NaiveDate};
use google_sheets4::{oauth2, Sheets};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

// ==========================================
// 1. Error Domain Definitions
// ==========================================

#[derive(Debug, Error)]
pub enum AppError {
    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),

    #[error("Regex compilation error: {0}")]
    Regex(#[from] regex::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Google API client error: {0}")]
    GoogleApi(String),

    #[error("Service account credentials not configured in environment")]
    CredentialsNotFound,
}

pub type Result<T> = std::result::Result<T, AppError>;

// ==========================================
// 2. Sheet / Date Helpers
// ==========================================

fn cell_to_string(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn extract_row_number(token: &str) -> String {
    token.chars().filter(|c| c.is_ascii_digit()).collect()
}

fn resolve_sheet_range(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return "WORK!A2:I".to_string();
    }

    let clean_input = trimmed
        .split_once('!')
        .map(|(_, rest)| rest.trim())
        .unwrap_or(trimmed);

    if let Some((start_part, end_part)) = clean_input.split_once(':') {
        let start_row = extract_row_number(start_part);
        let end_row = extract_row_number(end_part);

        match (start_row.is_empty(), end_row.is_empty()) {
            (false, false) => format!("WORK!A{start_row}:I{end_row}"),
            (false, true) => format!("WORK!A{start_row}:I"),
            _ => format!("WORK!{clean_input}"),
        }
    } else {
        let start_row = extract_row_number(clean_input);
        if !start_row.is_empty() {
            format!("WORK!A{start_row}:I")
        } else {
            format!("WORK!{clean_input}:I")
        }
    }
}

fn get_target_range() -> Result<String> {
    let args: Vec<String> = env::args().collect();

    if args.len() > 1 {
        let parsed = resolve_sheet_range(&args[1]);
        println!("Using CLI argument range: {parsed}");
        return Ok(parsed);
    }

    print!("Default 'WORK!A2:I'. [Enter] to accept, or input range (e.g., C12:I14): ");
    io::stdout().flush()?;

    let mut user_input = String::new();
    io::stdin().read_line(&mut user_input)?;

    let parsed = resolve_sheet_range(&user_input);
    println!("Selected target range: {parsed}");
    Ok(parsed)
}

fn parse_valid_date(date_str: &str) -> Option<NaiveDate> {
    let formats = [
        "%Y-%m-%d",
        "%b %d, %Y",
        "%d/%m/%Y",
        "%m/%d/%Y",
        "%d-%m-%Y",
    ];

    for fmt in &formats {
        if let Ok(d) = NaiveDate::parse_from_str(date_str.trim(), fmt) {
            return Some(d);
        }
    }

    None
}

fn calculate_validity(receive_date_str: Option<&str>, expire_date_str: Option<&str>) -> String {
    let exp_str = match expire_date_str {
        Some(s) if !s.trim().is_empty() && s != "MISSING" => s,
        _ => return "N/A".to_string(),
    };

    let rec_str = match receive_date_str {
        Some(s) if !s.trim().is_empty() => s,
        _ => return "N/A".to_string(),
    };

    let rec_date = parse_valid_date(rec_str);
    let exp_date = parse_valid_date(exp_str);

    if let (Some(start), Some(end)) = (rec_date, exp_date) {
        if end <= start {
            return "Expired".to_string();
        }

        // Kept intentionally close to the original implementation.
        // The day borrow uses a 30-day month approximation.
        let mut years = end.year() - start.year();
        let mut months = end.month() as i32 - start.month() as i32;
        let mut days = end.day() as i32 - start.day() as i32;

        if days < 0 {
            months -= 1;
            days += 30;
        }

        if months < 0 {
            years -= 1;
            months += 12;
        }

        let mut parts = Vec::new();

        if years > 0 {
            parts.push(format!(
                "{years} Year{}",
                if years > 1 { "s" } else { "" }
            ));
        }

        if months > 0 {
            parts.push(format!(
                "{months} Month{}",
                if months > 1 { "s" } else { "" }
            ));
        }

        if days > 0 {
            parts.push(format!(
                "{days} Day{}",
                if days > 1 { "s" } else { "" }
            ));
        }

        if parts.is_empty() {
            "0 Days".to_string()
        } else {
            parts.join(", ")
        }
    } else {
        "N/A".to_string()
    }
}

// ==========================================
// 3. Path & String Sanitization
// ==========================================

pub fn sanitize_path_component(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut last_was_space = false;

    for c in input.chars() {
        let replacement = match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        };

        if replacement.is_whitespace() {
            if !last_was_space {
                result.push(' ');
                last_was_space = true;
            }
        } else {
            result.push(replacement);
            last_was_space = false;
        }
    }

    result.trim().to_string()
}

// ==========================================
// 4. Name Parsing Engine
// ==========================================

const TITLES: &[&str] = &[
    "MR", "MRS", "MS", "MISS", "MSTR", "MASTER", "DR", "PROF", "REV", "MX", "INF",
    "INFANT", "BABY", "CHD", "CHILD",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedName {
    pub title: Option<String>,
    pub first_name: String,
    pub last_name: String,
    pub full_name: String,
    pub name_without_title: String,
    pub inverted_name: String,
    pub is_csv_formatted: bool,
}

pub fn parse_and_normalize_name(raw_name: &str) -> ParsedName {
    let name = raw_name.trim();

    if name.is_empty() {
        return ParsedName {
            title: None,
            first_name: String::new(),
            last_name: String::new(),
            full_name: "Unknown".to_string(),
            name_without_title: "Unknown".to_string(),
            inverted_name: "Unknown".to_string(),
            is_csv_formatted: false,
        };
    }

    let is_csv = name.contains(',');
    let mut title: Option<String> = None;

    let (clean_first, clean_last) = if is_csv {
        let (last_part, remaining) = name
            .split_once(',')
            .map(|(l, r)| (l.trim(), r.trim()))
            .unwrap_or((name, ""));

        let mut tokens: Vec<&str> = remaining.split_whitespace().collect();

        if let Some(&first_token) = tokens.first() {
            let clean_trimmed = first_token.to_uppercase();
            let clean_trimmed = clean_trimmed.trim_matches('.');
            if TITLES.contains(&clean_trimmed) {
                title = Some(clean_trimmed.to_string());
                tokens.remove(0);
            }
        }

        if title.is_none() {
            if let Some(&last_token) = tokens.last() {
                let clean_trimmed = last_token.to_uppercase();
                let clean_trimmed = clean_trimmed.trim_matches('.');
                if TITLES.contains(&clean_trimmed) {
                    title = Some(clean_trimmed.to_string());
                    tokens.pop();
                }
            }
        }

        (tokens.join(" "), last_part.to_string())
    } else {
        let mut tokens: Vec<&str> = name.split_whitespace().collect();

        if let Some(&first_token) = tokens.first() {
            let clean_trimmed = first_token.to_uppercase();
            let clean_trimmed = clean_trimmed.trim_matches('.');
            if TITLES.contains(&clean_trimmed) {
                title = Some(clean_trimmed.to_string());
                tokens.remove(0);
            }
        }

        match tokens.as_slice() {
            [] => (String::new(), String::new()),
            [single] => ((*single).to_string(), String::new()),
            [head @ .., tail] => (head.join(" "), (*tail).to_string()),
        }
    };

    let name_without_title = match (!clean_first.is_empty(), !clean_last.is_empty()) {
        (true, true) => format!("{clean_first} {clean_last}"),
        (true, false) => clean_first.clone(),
        (false, true) => clean_last.clone(),
        (false, false) => name.to_string(),
    };

    let full_name = if let Some(ref t) = title {
        format!("{t} {name_without_title}")
    } else {
        name_without_title.clone()
    };

    let inverted_name = match (!clean_last.is_empty(), !clean_first.is_empty()) {
        (true, true) => format!("{clean_last}, {clean_first}"),
        (true, false) => clean_last.to_string(),
        (false, true) => clean_first.to_string(),
        (false, false) => name.to_string(),
    };

    ParsedName {
        title,
        first_name: clean_first,
        last_name: clean_last,
        full_name,
        name_without_title,
        inverted_name,
        is_csv_formatted: is_csv,
    }
}

// ==========================================
// 5. Passport Metadata Extraction Engine
// ==========================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassportMetadata {
    /// The original Sheet cell, preserved exactly enough for troubleshooting.
    pub raw_text: String,

    /// First passport identifier found; this is the primary passport.
    pub primary_passport: Option<String>,

    /// Remaining passport identifiers after the primary passport.
    pub additional_passports: Vec<String>,

    /// All detected passport identifiers, kept for backward-friendly JSON/debugging.
    pub passports: Vec<String>,

    /// Quantity explicitly received in inventory, e.g. "-03 Passport" -> 3.
    pub passport_count: usize,

    /// Human-facing primary value used by folder/info generation.
    /// This is never an actual invented passport number.
    pub clean_passport: String,
}

pub struct PassportExtractor;

impl PassportExtractor {
    fn passport_regex() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"\b([a-zA-Z]{1,2}\d{7,8})\b").unwrap())
    }

    fn count_suffix_regex() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| {
            Regex::new(r"(?i)[-_,]\s*(\d{1,2})\s*(?:PASSPORT|PP)?").unwrap()
        })
    }

    fn strip_passport_label(input: &str) -> String {
        let without_count = Self::count_suffix_regex().replace(input, "");
        without_count
            .replace("Passport", "")
            .replace("PASSPORT", "")
            .replace("passport", "")
            .trim()
            .to_string()
    }

    pub fn extract(raw_text: &str) -> PassportMetadata {
        let raw = raw_text.trim().to_string();

        let passports: Vec<String> = Self::passport_regex()
            .find_iter(&raw)
            .map(|m| m.as_str().to_uppercase())
            .collect();

        let explicit_count = Self::count_suffix_regex()
            .captures(&raw)
            .and_then(|caps| caps.get(1))
            .and_then(|m| m.as_str().parse::<usize>().ok())
            .filter(|&c| (1..=16).contains(&c));

        let passport_count = explicit_count.unwrap_or(passports.len());

        let primary_passport = passports.first().cloned();
        let additional_passports = passports.iter().skip(1).cloned().collect::<Vec<_>>();

        let clean_passport = match &primary_passport {
            Some(primary) => primary.clone(),
            None => {
                let stripped = Self::strip_passport_label(&raw);
                if stripped.is_empty() || stripped.eq_ignore_ascii_case("passport") {
                    "MISSING".to_string()
                } else {
                    // Keep non-passport text available to the caller, but do not
                    // pretend it is a passport identifier.
                    stripped
                }
            }
        };

        PassportMetadata {
            raw_text: raw,
            primary_passport,
            additional_passports,
            passports,
            passport_count,
            clean_passport,
        }
    }
}

// ==========================================
// 6. Domain Models
// ==========================================

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ApplicantRole {
    #[default]
    SelfRole,
    Head(String),
    Member(String),
}

impl ApplicantRole {
    pub fn from_str(s: &str) -> Self {
        let upper = s.trim().to_uppercase();

        match upper.as_str() {
            "SELF" | "PRIMARY" | "HEAD" | "MAIN" | "SE" | "0" | "00" | "AA" | "A0"
            | "B0" | "C0" | "AAA" | "MAN" => Self::Head(upper),
            "" => Self::SelfRole,
            _ => Self::Member(upper),
        }
    }

    pub fn is_head(&self) -> bool {
        matches!(self, Self::Head(_) | Self::SelfRole)
    }

    pub fn is_sponsor(&self) -> bool {
        self.as_str().eq_ignore_ascii_case("SPONSOR")
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::SelfRole => "SELF",
            Self::Head(s) | Self::Member(s) => s.as_str(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FamilyType {
    SinglePerson,
    ApplicantFamily,
    SponsorFamily,
}

impl FamilyType {
    pub fn determine(family_id: Option<&str>, members: &[Applicant]) -> Self {
        if members.len() == 1 && family_id.is_none() {
            return Self::SinglePerson;
        }

        if members.iter().any(|member| member.role.is_sponsor()) {
            return Self::SponsorFamily;
        }

        Self::ApplicantFamily
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SinglePerson => "Single Person",
            Self::ApplicantFamily => "Applicant Family",
            Self::SponsorFamily => "Sponsor Family",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Applicant {
    pub sl: Option<String>,
    pub raw_name: String,
    pub parsed_name: ParsedName,
    pub raw_passport: String,
    pub passport_meta: PassportMetadata,
    pub receive_date: Option<String>,
    pub expired_date: Option<String>,
    pub country: Option<String>,
    pub role: ApplicantRole,
    pub family_id: Option<String>,
    pub remarks: Option<String>,
    pub validity: Option<String>,
}

impl Applicant {
    pub fn sheet_entry(&self) -> String {
        format!(
            "{}\t{}",
            self.parsed_name.name_without_title, self.raw_passport
        )
    }

    pub fn primary_passport(&self) -> Option<&str> {
        self.passport_meta.primary_passport.as_deref()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FamilyCase {
    pub family_id: Option<String>,
    pub family_type: FamilyType,
    pub received_date: Option<String>,
    pub country: Option<String>,
    pub members: Vec<Applicant>,
}

impl FamilyCase {
    pub fn new(family_id: Option<String>, member: Applicant) -> Self {
        let received_date = member.receive_date.clone();
        let country = member.country.clone();

        Self {
            family_id,
            family_type: FamilyType::SinglePerson,
            received_date,
            country,
            members: vec![member],
        }
    }

    pub fn refresh_metadata(&mut self) {
        self.family_type = FamilyType::determine(self.family_id.as_deref(), &self.members);

        self.received_date = self
            .members
            .iter()
            .filter_map(|m| m.receive_date.as_deref())
            .filter_map(parse_valid_date)
            .min()
            .map(|d| d.format("%Y-%m-%d").to_string())
            .or_else(|| self.members.first().and_then(|m| m.receive_date.clone()));

        self.country = self
            .members
            .iter()
            .find(|m| m.role.is_head())
            .and_then(|m| m.country.clone())
            .or_else(|| self.members.first().and_then(|m| m.country.clone()));
    }

    pub fn primary_member(&self) -> &Applicant {
        self.members
            .iter()
            .find(|m| {
                let role = m.role.as_str();
                role.eq_ignore_ascii_case("MAIN")
                    || role.eq_ignore_ascii_case("PRIMARY")
                    || role.eq_ignore_ascii_case("SELF")
                    || m.role.is_head()
            })
            .unwrap_or(&self.members[0])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FamilyGroupData {
    pub family_id: String,
    pub family_type: FamilyType,
    pub processed_date: String,
    pub received_date: Option<String>,
    pub country: Option<String>,
    pub total_members: usize,
    pub total_passports: usize,
    pub members: Vec<Applicant>,
}

// ==========================================
// 7. Sheet Row -> Applicant
// ==========================================

fn parse_applicant_row(row: &[Value]) -> Option<Applicant> {
    let get_col = |idx: usize| -> Option<String> { row.get(idx).and_then(cell_to_string) };

    let sl = get_col(0);
    let receive_date = get_col(1);
    let raw_name = get_col(2).unwrap_or_default();
    let raw_passport = get_col(3).unwrap_or_default();

    let raw_expired = get_col(4);
    let expired_date = match raw_expired {
        Some(ref s) if parse_valid_date(s).is_some() => s.clone(),
        _ => "MISSING".to_string(),
    };

    let country = get_col(5);

    // Preserve the original fallback: if column 6 is empty and column 4
    // contains non-date text, treat that text as the role.
    let raw_role = get_col(6).or_else(|| {
        get_col(4).filter(|s| parse_valid_date(s).is_none())
    });

    let role = raw_role
        .map(|s| ApplicantRole::from_str(&s))
        .unwrap_or_default();

    let family_id = get_col(7).filter(|s| !s.trim().is_empty());
    let remarks = get_col(8);

    // Ignore completely empty rows. A row with only a family id is still kept,
    // matching the original behavior.
    if raw_name.is_empty() && raw_passport.is_empty() && family_id.is_none() {
        return None;
    }

    let parsed_name = parse_and_normalize_name(&raw_name);
    let passport_meta = PassportExtractor::extract(&raw_passport);
    let validity = calculate_validity(receive_date.as_deref(), Some(&expired_date));

    Some(Applicant {
        sl,
        raw_name,
        parsed_name,
        raw_passport,
        passport_meta,
        receive_date,
        expired_date: Some(expired_date),
        country,
        role,
        family_id,
        remarks,
        validity: Some(validity),
    })
}

fn group_rows_into_cases(rows: Vec<Vec<Value>>) -> Vec<FamilyCase> {
    let mut cases: Vec<FamilyCase> = Vec::new();
    let mut family_indexes: HashMap<String, usize> = HashMap::new();

    for row in rows {
        let Some(applicant) = parse_applicant_row(&row) else {
            continue;
        };

        match applicant.family_id.clone() {
            Some(family_id) => {
                if let Some(&index) = family_indexes.get(&family_id) {
                    cases[index].members.push(applicant);
                    cases[index].refresh_metadata();
                } else {
                    let mut case = FamilyCase::new(Some(family_id.clone()), applicant);
                    case.refresh_metadata();
                    family_indexes.insert(family_id, cases.len());
                    cases.push(case);
                }
            }
            None => {
                // Every no-family-id row becomes its own one-person case.
                let mut case = FamilyCase::new(None, applicant);
                case.refresh_metadata();
                cases.push(case);
            }
        }
    }

    cases
}

// ==========================================
// 8. Filesystem Service & Folder Builder
// ==========================================

const SINGLE_SUBDIRS: &[&str] = &[
    "apply",
    "documents",
    "bank",
    "combine",
    "epson",
    "scan-copy",
    "delete-me",
    "letters",
    "data",
];

const FAMILY_ROOT_SUBDIRS: &[&str] = &["apply", "bank", "delete-me", "letters", "data", "extra"];

const FAMILY_MEMBER_SUBDIRS: &[&str] = &[
    "documents",
    "combine",
    "epson",
    "scan-copy",
];

pub struct FolderBuilder {
    processed_date: String,
}

impl FolderBuilder {
    pub fn new(processed_date: String) -> Self {
        Self { processed_date }
    }

    const FILE_WRITE_RETRIES: usize = 3;
    const FILE_WRITE_RETRY_DELAY_MS: u64 = 150;

    fn month_root(&self, case: &FamilyCase) -> PathBuf {
        let date = case
            .received_date
            .as_deref()
            .and_then(parse_valid_date)
            .unwrap_or_else(|| {
                parse_valid_date(&self.processed_date).unwrap_or_else(|| Local::now().date_naive())
            });

        PathBuf::from(date.format("%Y-%B").to_string())
    }

    fn country_prefix(&self, country: Option<&str>) -> String {
        country
            .filter(|c| !c.trim().is_empty())
            .map(|c| format!("{}@", c.trim()))
            .unwrap_or_default()
    }

    fn single_folder_name(&self, case: &FamilyCase) -> String {
        let applicant = case.primary_member();
        let passport = applicant.primary_passport().unwrap_or("");
        let passport_part = if passport.is_empty() {
            "MISSING".to_string()
        } else {
            passport.to_string()
        };

        sanitize_path_component(&format!(
            "{}{} {}",
            self.country_prefix(case.country.as_deref()),
            applicant.parsed_name.full_name,
            passport_part
        ))
    }

    fn family_folder_name(&self, case: &FamilyCase) -> String {
        let anchor = case.primary_member();
        let family_id = case
            .family_id
            .as_deref()
            .unwrap_or("NO-FAMILY-ID");

        let type_label = match case.family_type {
            FamilyType::SponsorFamily => "SPONSOR FAMILY",
            _ => "FAMILY",
        };

        sanitize_path_component(&format!(
            "{}{} . {} {} Pax {}",
            self.country_prefix(case.country.as_deref()),
            anchor.parsed_name.full_name,
            type_label,
            case.members.len(),
            family_id
        ))
    }

    fn case_folder_path(&self, case: &FamilyCase) -> PathBuf {
        let root = self.month_root(case);

        match case.family_type {
            FamilyType::SinglePerson => root.join(self.single_folder_name(case)),
            FamilyType::ApplicantFamily | FamilyType::SponsorFamily => {
                root.join(self.family_folder_name(case))
            }
        }
    }

    fn member_folder_name(member: &Applicant) -> String {
        let passport = member.primary_passport().unwrap_or("");
        let passport_part = if passport.is_empty() {
            "MISSING".to_string()
        } else {
            passport.to_string()
        };

        sanitize_path_component(&format!(
            "{} - {} {}",
            member.role.as_str(), member.parsed_name.full_name, passport_part
        ))
    }

    pub async fn create_case(&self, case: &FamilyCase) -> Result<()> {
        let base_path = self.case_folder_path(case);

        fs::create_dir_all(&base_path).await?;

        match case.family_type {
            FamilyType::SinglePerson => self.create_single_case(&base_path, case).await?,
            FamilyType::ApplicantFamily | FamilyType::SponsorFamily => {
                self.create_family_case(&base_path, case).await?
            }
        }

        Ok(())
    }

    async fn create_single_case(&self, base_path: &Path, case: &FamilyCase) -> Result<()> {
        let applicant = case.primary_member();

        self.create_subdirectories(base_path, SINGLE_SUBDIRS).await?;
        self.write_remarks_if_present(base_path, applicant.remarks.as_deref())
            .await?;
        self.write_applicant_info_file(base_path, applicant).await?;

        let json_path = base_path.join("data").join("client_data.json");
        self.save_json_file(&json_path, applicant).await?;

        self.verify_info_file(base_path).await
    }

    async fn create_family_case(&self, base_path: &Path, case: &FamilyCase) -> Result<()> {
        self.create_subdirectories(base_path, FAMILY_ROOT_SUBDIRS)
            .await?;

        // Members are intentionally processed sequentially, preserving the
        // reliability-first behavior of the original implementation.
        for member in &case.members {
            let member_path = base_path.join(Self::member_folder_name(member));
            fs::create_dir_all(&member_path).await?;

            self.create_subdirectories(&member_path, FAMILY_MEMBER_SUBDIRS)
                .await?;
            self.write_remarks_if_present(&member_path, member.remarks.as_deref())
                .await?;
            self.write_applicant_info_file(&member_path, member).await?;
            self.verify_info_file(&member_path).await?;
        }

        let family_payload = FamilyGroupData {
            family_id: case
                .family_id
                .clone()
                .unwrap_or_else(|| "NO-FAMILY-ID".to_string()),
            family_type: case.family_type.clone(),
            processed_date: self.processed_date.clone(),
            received_date: case.received_date.clone(),
            country: case.country.clone(),
            total_members: case.members.len(),
            total_passports: case
                .members
                .iter()
                .map(|m| m.passport_meta.passport_count)
                .sum(),
            members: case.members.clone(),
        };

        self.write_family_info_file(base_path, case).await?;

        let family_json_path = base_path.join("data").join("family_data.json");
        self.save_json_file(&family_json_path, &family_payload).await?;

        self.verify_info_file(base_path).await
    }

    async fn create_subdirectories(&self, path: &Path, subdirs: &[&str]) -> Result<()> {
        for subdir in subdirs {
            fs::create_dir_all(path.join(subdir)).await?;
        }

        Ok(())
    }

    async fn write_remarks_if_present(
        &self,
        folder_path: &Path,
        remarks: Option<&str>,
    ) -> Result<()> {
        if let Some(content) = remarks {
            if !content.trim().is_empty() {
                let file_path = folder_path.join("remarks.txt");
                self.write_text_file_reliable(&file_path, content).await?;
            }
        }

        Ok(())
    }

    async fn write_applicant_info_file(
        &self,
        folder_path: &Path,
        applicant: &Applicant,
    ) -> Result<()> {
        let file_path = folder_path.join("info.txt");
        let sl = applicant.sl.as_deref().unwrap_or("N/A");
        let remarks_str = applicant.remarks.as_deref().unwrap_or("NONE");
        let expired_date_str = applicant.expired_date.as_deref().unwrap_or("MISSING");
        let validity_str = applicant.validity.as_deref().unwrap_or("N/A");
        let primary_passport = applicant.primary_passport().unwrap_or("MISSING");
        let additional_passports = if applicant.passport_meta.additional_passports.is_empty() {
            "NONE".to_string()
        } else {
            applicant.passport_meta.additional_passports.join(", ")
        };
        let received_count = applicant.passport_meta.passport_count;

        let mut buffer = String::with_capacity(768);
        let _ = writeln!(
            buffer,
            "==========================================\n\
INFORMATION AND SUMMARY\n\
==========================================\n\
SL              : {}\n\
Processed Date  : {}\n\
Role            : {}\n\
Remarks         : {}\n\n\
------------------ Record ------------------\n\
   Name Record     : {}\n\
   Full Name       : {}\n\
   Inverted        : {}\n\
   Passport        : {}\n\
   Other Passports : {}\n\
   Passport Count  : {}\n\
   Expired Date    : {}\n\
   Validity        : {}\n\
   Sheet Entry     : {}\n\n\
----------------- By Santo -----------------",
            sl,
            self.processed_date,
            applicant.role.as_str(),
            remarks_str,
            applicant.raw_name,
            applicant.parsed_name.full_name,
            applicant.parsed_name.inverted_name,
            primary_passport,
            additional_passports,
            received_count,
            expired_date_str,
            validity_str,
            applicant.sheet_entry(),
        );

        self.write_text_file_reliable(&file_path, &buffer).await
    }

    async fn write_family_info_file(&self, folder_path: &Path, case: &FamilyCase) -> Result<()> {
        let file_path = folder_path.join("info.txt");
        let family_id = case.family_id.as_deref().unwrap_or("NONE");
        let total_passports: usize = case
            .members
            .iter()
            .map(|m| m.passport_meta.passport_count)
            .sum();

        let mut member_summary = String::with_capacity(512 * case.members.len());

        for (idx, member) in case.members.iter().enumerate() {
            let sl = member.sl.as_deref().unwrap_or("N/A");
            let remarks_str = member.remarks.as_deref().unwrap_or("NONE");
            let expired_date_str = member.expired_date.as_deref().unwrap_or("MISSING");
            let validity_str = member.validity.as_deref().unwrap_or("N/A");
            let primary_passport = member.primary_passport().unwrap_or("MISSING");

            let additional = if member.passport_meta.additional_passports.is_empty() {
                "NONE".to_string()
            } else {
                member.passport_meta.additional_passports.join(", ")
            };

            let _ = writeln!(
                member_summary,
                "[Number: {}]\n\
   Role            : #{}\n\
   SL              : {}\n\
   Remarks         : {}\n\
   Name Record     : {}\n\
   Full Name       : {}\n\
   Inverted        : {}\n\
   Passport        : {}\n\
   Other Passports : {}\n\
   Passport Count  : {}\n\
   Expired Date    : {}\n\
   Validity        : {}\n\
   Sheet Entry     : {}\n",
                idx + 1,
                member.role.as_str(),
                sl,
                remarks_str,
                member.raw_name,
                member.parsed_name.full_name,
                member.parsed_name.inverted_name,
                primary_passport,
                additional,
                member.passport_meta.passport_count,
                expired_date_str,
                validity_str,
                member.sheet_entry()
            );
        }

        let mut content = String::with_capacity(768 + member_summary.len());
        let _ = writeln!(
            content,
            "==========================================\n\
INFORMATION AND SUMMARY\n\
==========================================\n\
Family ID       : {}\n\
Family Type     : {}\n\
Processed Date  : {}\n\
Received Date   : {}\n\
Country         : {}\n\
Total Members   : {}\n\
Total Passports : {}\n\n\
------------------ Record ------------------\n\
{}\n\n\
----------------- By Santo -----------------",
            family_id,
            case.family_type.as_str(),
            self.processed_date,
            case.received_date.as_deref().unwrap_or("MISSING"),
            case.country.as_deref().unwrap_or("MISSING"),
            case.members.len(),
            total_passports,
            member_summary.trim_end(),
        );

        self.write_text_file_reliable(&file_path, &content).await
    }

    async fn verify_info_file(&self, folder_path: &Path) -> Result<()> {
        let info_path = folder_path.join("info.txt");

        if !fs::try_exists(&info_path).await? {
            return Err(AppError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("info.txt was not found after creation: {}", info_path.display()),
            )));
        }

        Ok(())
    }

    // ------------------------------------------
    // Centralized reliable text writer
    // ------------------------------------------

    async fn write_text_file_reliable(&self, file_path: &Path, content: &str) -> Result<()> {
        let max_attempts = Self::FILE_WRITE_RETRIES + 1;

        for attempt in 1..=max_attempts {
            let write_result: std::io::Result<()> = async {
                let mut file = File::create(file_path).await?;
                file.write_all(content.as_bytes()).await?;
                file.flush().await?;
                file.sync_all().await?;
                drop(file);

                if !fs::try_exists(file_path).await? {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "file path does not exist after write",
                    ));
                }

                let metadata = fs::metadata(file_path).await?;
                if metadata.len() == 0 && !content.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "file exists but is empty after write",
                    ));
                }

                Ok(())
            }
            .await;

            match write_result {
                Ok(()) => return Ok(()),
                Err(err) => {
                    let retryable = matches!(
                        err.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::PermissionDenied
                            | io::ErrorKind::NotFound
                    );

                    if retryable && attempt < max_attempts {
                        eprintln!(
                            "[WARN] File write attempt {attempt}/{max_attempts} failed for '{}': {err}. Retrying...",
                            file_path.display()
                        );

                        sleep(Duration::from_millis(Self::FILE_WRITE_RETRY_DELAY_MS)).await;
                        continue;
                    }

                    return Err(AppError::Io(io::Error::new(
                        err.kind(),
                        format!("Failed writing '{}': {err}", file_path.display()),
                    )));
                }
            }
        }

        Err(AppError::Io(io::Error::new(
            io::ErrorKind::Other,
            "Text-file write helper exited unexpectedly",
        )))
    }

    async fn save_json_file<T: Serialize>(&self, target_path: &Path, data: &T) -> Result<()> {
        let parent = target_path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).await?;

        let temp_path = parent.join(format!(
            ".tmp_{}",
            target_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ));

        let payload = serde_json::to_vec_pretty(data)?;

        let mut file = File::create(&temp_path).await?;
        file.write_all(&payload).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        fs::rename(&temp_path, target_path).await?;

        if !fs::try_exists(target_path).await? {
            return Err(AppError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("JSON file was not found after rename: {}", target_path.display()),
            )));
        }

        Ok(())
    }
}

// ==========================================
// 9. Sheet Processor Engine
// ==========================================

pub struct ProcessingSummary {
    pub cases_found: usize,
    pub single_cases: usize,
    pub applicant_families: usize,
    pub sponsor_families: usize,
    pub cases_success: usize,
    pub cases_failed: usize,
}

pub struct SheetProcessor {
    builder: FolderBuilder,
}

impl SheetProcessor {
    pub fn new(processed_date: String) -> Self {
        Self {
            builder: FolderBuilder::new(processed_date),
        }
    }

    pub async fn process_rows(&self, rows: Vec<Vec<Value>>) -> ProcessingSummary {
        if rows.is_empty() {
            println!("No rows to process.");
            return ProcessingSummary {
                cases_found: 0,
                single_cases: 0,
                applicant_families: 0,
                sponsor_families: 0,
                cases_success: 0,
                cases_failed: 0,
            };
        }

        let cases = group_rows_into_cases(rows);

        let single_cases = cases
            .iter()
            .filter(|case| matches!(case.family_type, FamilyType::SinglePerson))
            .count();
        let applicant_families = cases
            .iter()
            .filter(|case| matches!(case.family_type, FamilyType::ApplicantFamily))
            .count();
        let sponsor_families = cases
            .iter()
            .filter(|case| matches!(case.family_type, FamilyType::SponsorFamily))
            .count();

        let mut cases_success = 0usize;
        let mut cases_failed = 0usize;

        for case in cases {
            let display_name = case.primary_member().parsed_name.full_name.clone();
            let case_label = case
                .family_id
                .as_deref()
                .unwrap_or("SINGLE");

            println!(
                "[CREATE] {}: {} ({} member{})",
                case.family_type.as_str(),
                case_label,
                case.members.len(),
                if case.members.len() == 1 { "" } else { "s" }
            );
            println!("         Anchor: {display_name}");

            match self.builder.create_case(&case).await {
                Ok(()) => {
                    cases_success += 1;
                    println!("[OK] Case completed: {case_label}");
                }
                Err(err) => {
                    cases_failed += 1;
                    eprintln!("[ERROR] Failed processing case {case_label}: {err}");
                }
            }
        }

        println!("==========================================");
        println!("  Cases Found                 : {}", cases_success + cases_failed);
        println!("  Single Person Cases         : {single_cases}");
        println!("  Applicant Families          : {applicant_families}");
        println!("  Sponsor Families            : {sponsor_families}");
        println!("  Cases Success               : {cases_success}");
        println!("  Cases Failed                : {cases_failed}");
        println!("==========================================\n");

        ProcessingSummary {
            cases_found: cases_success + cases_failed,
            single_cases,
            applicant_families,
            sponsor_families,
            cases_success,
            cases_failed,
        }
    }
}

// ==========================================
// 10. Runtime Entry Point
// ==========================================

#[tokio::main]
async fn main() -> Result<()> {
    let range = get_target_range()?;
    let processed_date = Local::now().format("%Y-%m-%d").to_string();
    let processor = SheetProcessor::new(processed_date);

    let secret = if let Ok(json_str) = env::var("GOOGLE_SERVICE_ACCOUNT_JSON") {
        oauth2::parse_service_account_key(json_str)
            .map_err(|e| AppError::GoogleApi(format!("Failed to parse environment credentials: {e}")))?
    } else {
        let creds_path = env::var("GOOGLE_APPLICATION_CREDENTIALS")
            .map_err(|_| AppError::CredentialsNotFound)?;
        oauth2::read_service_account_key(creds_path)
            .await
            .map_err(|e| {
                AppError::GoogleApi(format!("Failed to read service account key file: {e}"))
            })?
    };

    let authenticator = oauth2::ServiceAccountAuthenticator::builder(secret)
        .build()
        .await
        .map_err(|e| AppError::GoogleApi(e.to_string()))?;

    let client = hyper::Client::builder().build(
        hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .https_or_http()
            .enable_http1()
            .build(),
    );

    let hub = Sheets::new(client, authenticator);

    let spreadsheet_id = "1kdC06KMA4JqFkgDod2AptaMJ5i0duWSx8TB7VW1xcu0";

    let mut retries = 5;
    let mut backoff_duration = Duration::from_secs(2);

    let response = loop {
        match hub
            .spreadsheets()
            .values_get(spreadsheet_id, &range)
            .doit()
            .await
        {
            Ok(res) => break res,
            Err(e) if retries > 0 => {
                eprintln!(
                    "[WARN] Google Sheets API request failure: {e}. Retrying in {backoff_duration:?}... (Retries left: {retries})"
                );
                sleep(backoff_duration).await;
                retries -= 1;
                backoff_duration *= 2;
            }
            Err(e) => return Err(AppError::GoogleApi(e.to_string())),
        }
    };

    if let Some(rows) = response.1.values {
        if !rows.is_empty() {
            processor.process_rows(rows).await;
            return Ok(());
        }
    }

    println!("No record rows retrieved from the specified spreadsheet range.");
    Ok(())
}
