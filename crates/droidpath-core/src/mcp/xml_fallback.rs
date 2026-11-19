use quick_xml::events::Event;
use quick_xml::Reader;

use crate::protocol::ShadowCompactNode;

/// Cold-start fallback XML parser for `adb shell uiautomator dump`.
pub struct DroidPathUITreeFallbackParser;

impl DroidPathUITreeFallbackParser {
    pub fn parse(xml_string: &str) -> Vec<ShadowCompactNode> {
        let Some(start) = xml_string.find("<hierarchy") else {
            return Vec::new();
        };
        let Some(end_rel) = xml_string[start..].rfind("</hierarchy>") else {
            return Vec::new();
        };
        let xml_slice = &xml_string[start..start + end_rel + "</hierarchy>".len()];

        let mut reader = Reader::from_str(xml_slice);
        let mut nodes = Vec::new();
        let mut node_index = 0i32;

        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.name().as_ref() == b"node" => {
                    let mut text = String::new();
                    let mut resource_id = String::new();
                    let mut content_desc = String::new();
                    let mut class_raw = String::new();
                    let mut clickable = false;
                    let mut scrollable = false;
                    let mut bounds = String::new();

                    for attr in e.attributes().flatten() {
                        let key = attr.key.as_ref();
                        let val = attr
                            .unescape_value()
                            .map(|v| v.into_owned())
                            .unwrap_or_else(|_| String::from_utf8_lossy(&attr.value).into_owned());
                        match key {
                            b"text" => text = val.trim().to_string(),
                            b"resource-id" => resource_id = val.trim().to_string(),
                            b"content-desc" => content_desc = val.trim().to_string(),
                            b"class" => class_raw = val,
                            b"clickable" => clickable = val == "true",
                            b"scrollable" => scrollable = val == "true",
                            b"bounds" => bounds = val,
                            _ => {}
                        }
                    }

                    let class_name = class_raw
                        .rsplit('.')
                        .next()
                        .unwrap_or("")
                        .to_string();

                    let is_meaningful = clickable
                        || scrollable
                        || !text.is_empty()
                        || !content_desc.is_empty()
                        || !resource_id.is_empty();
                    if !is_meaningful {
                        continue;
                    }
                    let Some((cx, cy)) = Self::parse_center(&bounds) else {
                        continue;
                    };

                    nodes.push(ShadowCompactNode {
                        index: node_index,
                        text,
                        resource_id,
                        content_desc,
                        class_name,
                        clickable,
                        scrollable,
                        bounds,
                        center_x: cx,
                        center_y: cy,
                    });
                    node_index += 1;
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }

        nodes
    }

    pub fn parse_center(bounds: &str) -> Option<(i32, i32)> {
        let mut nums = Vec::with_capacity(4);
        let mut cur = String::new();
        for ch in bounds.chars() {
            if ch.is_ascii_digit() {
                cur.push(ch);
            } else if !cur.is_empty() {
                if let Ok(v) = cur.parse::<i32>() {
                    nums.push(v);
                }
                cur.clear();
            }
        }
        if !cur.is_empty() {
            if let Ok(v) = cur.parse::<i32>() {
                nums.push(v);
            }
        }
        if nums.len() != 4 {
            return None;
        }
        Some(((nums[0] + nums[2]) / 2, (nums[1] + nums[3]) / 2))
    }
}
