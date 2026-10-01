use serde_json::Value;

/// Bump when prompts change so runs can be compared in the `analyses` table.
pub const IMAGE_PROMPT_VERSION: &str = "img-v1";
pub const VIDEO_MERGE_PROMPT_VERSION: &str = "video-merge-v1";

pub const IMAGE_SYSTEM: &str = "You are a precise visual cataloguing assistant. \
You describe only what is visible. You never guess names or identities of people. \
You always answer with a single JSON object and nothing else.";

pub const IMAGE_USER: &str = r#"Describe this image for a searchable archive. Answer with JSON in exactly this shape:
{
  "summary": "2-4 sentences describing the whole image",
  "people": [
    {"apparent_age": "child|teen|young adult|adult|senior", "gender_presentation": "...",
     "appearance": "hair, facial hair, glasses, build, skin tone", "clothing": "colors and garments",
     "action": "what the person is doing", "position": "where in the image"}
  ],
  "objects": ["notable objects"],
  "scene": "location type, indoor/outdoor, time of day, weather if visible",
  "visible_text": "any readable text, verbatim, empty string if none",
  "tags": ["5-15 short lowercase keywords"]
}
Use an empty list for "people" if nobody is visible. Be specific about clothing colors and distinctive features."#;

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
  "people": [{"appearance": "...", "clothing": "...", "action": "...", "seen_at": "timestamps"}],
  "objects": ["notable objects"],
  "scene": "locations and setting",
  "visible_text": "readable text seen in the video",
  "tags": ["5-15 short lowercase keywords"]
}
Merge descriptions that clearly refer to the same person."#,
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

/// Flattens the structured analysis into plain text for the summary column and FTS.
pub fn searchable_text(v: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = v["summary"].as_str().filter(|s| !s.is_empty()) {
        parts.push(s.trim().to_string());
    }
    if let Some(people) = v["people"].as_array().filter(|p| !p.is_empty()) {
        let descs: Vec<String> = people
            .iter()
            .map(|p| match p {
                Value::Object(map) => map
                    .values()
                    .filter_map(|x| x.as_str())
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
                    .join(", "),
                other => other.as_str().unwrap_or_default().to_string(),
            })
            .filter(|d| !d.is_empty())
            .collect();
        if !descs.is_empty() {
            parts.push(format!("People: {}.", descs.join("; ")));
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
    fn flattens_analysis() {
        let v = json!({
            "summary": "A man on a bike.",
            "people": [{"apparent_age": "adult", "clothing": "red jacket", "action": ""}],
            "objects": ["bicycle"],
            "scene": "street",
            "visible_text": "",
            "tags": ["cycling", "city"]
        });
        let t = searchable_text(&v);
        assert!(t.starts_with("A man on a bike."));
        assert!(t.contains("People: adult, red jacket."));
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
