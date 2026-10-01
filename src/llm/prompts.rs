use serde_json::Value;

/// Bump when prompts change so runs can be compared in the `analyses` table.
pub const IMAGE_PROMPT_VERSION: &str = "img-v2";
pub const VIDEO_MERGE_PROMPT_VERSION: &str = "video-merge-v2";

pub const IMAGE_SYSTEM: &str = "You are a precise visual cataloguing assistant. \
You describe only what is visible. You never guess names or identities of people. \
You always answer with a single JSON object and nothing else.";

pub const IMAGE_USER: &str = r#"Describe this image for a searchable archive. Answer with JSON in exactly this shape:
{
  "summary": "2-4 sentences describing the whole image",
  "people_count": 0,
  "people": [
    {"apparent_age": "child|teen|young adult|adult|senior", "gender_presentation": "...",
     "appearance": "hair, facial hair, glasses, build, skin tone", "clothing": "colors and garments",
     "action": "what the person is doing", "position": "where in the image"}
  ],
  "objects": ["notable objects"],
  "scene": "location type, indoor/outdoor, time of day, weather if visible",
  "visible_text": "readable text, empty string if none",
  "tags": ["5-15 short lowercase keywords"]
}
Rules:
- "people_count" is your estimate of all visible people (0 if nobody is visible).
- List at most 6 people in "people": the most prominent ones. Describe a crowd or group as ONE entry instead of listing every member.
- Be specific about clothing colors and distinctive features. Read name tags and labels if legible.
- "visible_text": at most 300 characters. Transcribe short text verbatim; for long text (code, menus, file lists, documents) give a short excerpt or describe it. Never repeat lines.
- Keep the whole answer short and complete; it must be valid JSON."#;

pub const VIDEO_MERGE_SYSTEM: &str = "You summarize videos from timestamped frame descriptions. \
You never guess names or identities of people. You always answer with a single JSON object and nothing else.";

pub fn video_merge_user(frames: &[(f64, String)]) -> String {
    let mut s = String::from(
        "These are descriptions of frames sampled from one video, with timestamps in seconds.\n\n",
    );
    for (ts, desc) in frames {
        s.push_str(&format!("[{}] {desc}\n", format_ts(*ts)));
    }
    s.push_str(
        r#"
Summarize the whole video. Answer with JSON in exactly this shape:
{
  "summary": "3-6 sentences: what happens, where, who appears (by description) and how it develops",
  "people_count": 0,
  "people": [{"appearance": "...", "clothing": "...", "action": "...", "seen_at": "timestamps"}],
  "objects": ["notable objects"],
  "scene": "locations and setting",
  "visible_text": "readable text seen in the video, at most 300 characters",
  "tags": ["5-15 short lowercase keywords"]
}
Rules:
- Merge descriptions that clearly refer to the same person.
- List at most 6 people; describe crowds or groups as one entry.
- "people_count" is the largest number of people visible at the same time.
- Keep the whole answer short and complete; it must be valid JSON."#,
    );
    s
}

fn format_ts(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        total / 60 % 60,
        total % 60
    )
}

/// Parses the model output as JSON, tolerating code fences and surrounding prose.
pub fn parse_json(text: &str) -> Option<Value> {
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(text) {
        return Some(v);
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    match serde_json::from_str::<Value>(text.get(start..=end)?) {
        Ok(v @ Value::Object(_)) => Some(v),
        _ => None,
    }
}

/// Recovers the `summary` string from a JSON answer that was cut off mid-way.
pub fn salvage_summary(text: &str) -> Option<String> {
    let key = text.find("\"summary\"")?;
    let rest = text[key + "\"summary\"".len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let summary: String = serde_json::Deserializer::from_str(rest)
        .into_iter::<String>()
        .next()?
        .ok()?;
    let summary = summary.trim();
    (!summary.is_empty()).then(|| summary.to_string())
}

/// Flattens the structured analysis into plain text for the summary column and FTS.
pub fn searchable_text(v: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = v["summary"].as_str().filter(|s| !s.is_empty()) {
        parts.push(s.trim().to_string());
    }
    if let Some(n) = v["people_count"].as_u64().filter(|n| *n > 0) {
        parts.push(format!("People visible: {n}"));
    }
    if let Some(people) = v["people"].as_array() {
        let lines: Vec<String> = people
            .iter()
            .map(describe_person)
            .filter(|d| !d.is_empty())
            .enumerate()
            .map(|(i, d)| format!("Person {}: {d}", i + 1))
            .collect();
        if !lines.is_empty() {
            parts.push(lines.join("\n"));
        }
    }
    for (key, label) in [("objects", "Objects"), ("tags", "Tags")] {
        if let Some(items) = v[key].as_array() {
            let list: Vec<&str> = items.iter().filter_map(|x| x.as_str()).collect();
            if !list.is_empty() {
                parts.push(format!("{label}: {}.", list.join(", ")));
            }
        }
    }
    for (key, label) in [("scene", "Scene"), ("visible_text", "Text")] {
        if let Some(s) = v[key].as_str().filter(|s| !s.trim().is_empty()) {
            parts.push(format!("{label}: {}", s.trim()));
        }
    }
    parts.join("\n")
}

/// Person fields in reading order; JSON object keys come back alphabetically sorted.
const PERSON_FIELDS: &[(&str, Option<&str>)] = &[
    ("apparent_age", None),
    ("gender_presentation", None),
    ("appearance", Some("appearance")),
    ("clothing", Some("clothing")),
    ("action", Some("action")),
    ("position", Some("position")),
    ("seen_at", Some("seen at")),
];

fn describe_person(p: &Value) -> String {
    let Value::Object(map) = p else {
        return p.as_str().unwrap_or_default().trim().to_string();
    };
    let text = |v: &Value| {
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut head = Vec::new();
    let mut labelled = Vec::new();
    for (key, label) in PERSON_FIELDS {
        if let Some(val) = map.get(*key).and_then(text) {
            match label {
                None => head.push(val),
                Some(l) => labelled.push(format!("{l}: {val}")),
            }
        }
    }
    for (key, val) in map {
        if !PERSON_FIELDS.iter().any(|(k, _)| k == key)
            && let Some(val) = text(val)
        {
            labelled.push(format!("{}: {val}", key.replace('_', " ")));
        }
    }
    let mut out = head.join(", ");
    if !labelled.is_empty() {
        if !out.is_empty() {
            out.push_str("; ");
        }
        out.push_str(&labelled.join("; "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_fenced_json() {
        let v = parse_json("Sure!\n```json\n{\"summary\": \"x\"}\n```").unwrap();
        assert_eq!(v["summary"], "x");
        assert!(parse_json("no json here").is_none());
        assert!(parse_json("[1, 2]").is_none());
    }

    #[test]
    fn salvages_truncated_summary() {
        let cut = "{\n  \"summary\": \"A crowd on a \\\"busy\\\" street.\",\n  \"people\": [{\"appearance\": \"dark";
        assert_eq!(
            salvage_summary(cut).as_deref(),
            Some("A crowd on a \"busy\" street.")
        );
        assert_eq!(salvage_summary("{\"summary\": \"unterminated"), None);
        assert_eq!(salvage_summary("no summary"), None);
    }

    #[test]
    fn flattens_analysis() {
        let v = json!({
            "summary": "A man on a bike.",
            "people_count": 3,
            "people": [
                {"position": "left", "gender_presentation": "male", "apparent_age": "adult",
                 "clothing": "red jacket", "action": "", "hat": "cap"},
                {"apparent_age": "child"},
                {}
            ],
            "objects": ["bicycle"],
            "scene": "street",
            "visible_text": "",
            "tags": ["cycling", "city"]
        });
        let t = searchable_text(&v);
        assert!(
            t.starts_with("A man on a bike.\nPeople visible: 3\n"),
            "{t}"
        );
        assert!(
            t.contains("Person 1: adult, male; clothing: red jacket; position: left; hat: cap\n"),
            "{t}"
        );
        assert!(t.contains("Person 2: child\n"), "{t}");
        assert!(!t.contains("Person 3"), "{t}");
        assert!(t.contains("Objects: bicycle."));
        assert!(t.contains("Tags: cycling, city."));
        assert!(t.contains("Scene: street"));
        assert!(!t.contains("Text:"));
    }

    #[test]
    fn merge_prompt_has_timestamps() {
        let p = video_merge_user(&[(0.0, "a".into()), (3725.0, "b".into())]);
        assert!(p.contains("[00:00:00] a"));
        assert!(p.contains("[01:02:05] b"));
    }
}
