// Output formatting follows Mobile-MCP 1.0.17 (Apache-2.0); see licenses/mobile-mcp-LICENSE.
use crate::model::Result;
use quick_xml::{events::Event, Reader};
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct Element {
    pub attrs: HashMap<String, String>,
    pub rect: [i32; 4], // x, y, width, height
    pub reference: String,
}
impl Element {
    pub fn text(&self) -> &str {
        self.attr("text")
    }
    pub fn attr(&self, key: &str) -> &str {
        self.attrs.get(key).map(String::as_str).unwrap_or("")
    }
    pub fn focused_editor(&self) -> bool {
        self.attr("focused") == "true"
            && (self.attr("class").contains("EditText") || self.attr("editable") == "true")
    }
    pub fn json(&self) -> Value {
        let mut v = json!({"ref": self.reference, "type": self.attr("class"), "text": self.text(),
            "label": self.attr("content-desc"), "identifier": self.attr("resource-id"),
            "coordinates": {"x":self.rect[0],"y":self.rect[1],"width":self.rect[2],"height":self.rect[3]}});
        for k in ["focused", "selected", "checked"] {
            if self.attr(k) == "true" {
                v[k] = json!(true);
            }
        }
        if self.attr("enabled") == "false" {
            v["enabled"] = json!(false);
        }
        v
    }
    fn line(&self) -> String {
        let class = self.attr("class");
        let mut p = vec![
            self.reference.clone(),
            class.rsplit('.').next().unwrap_or(class).into(),
        ];
        for (out, key) in [
            ("text", "text"),
            ("label", "content-desc"),
            ("id", "resource-id"),
        ] {
            if !self.attr(key).is_empty() {
                p.push(format!("{out}={}", json!(self.attr(key))));
            }
        }
        p.push(format!("at={},{}", self.rect[0], self.rect[1]));
        p.push(format!("size={}x{}", self.rect[2], self.rect[3]));
        for k in ["focused", "selected", "checked"] {
            if self.attr(k) == "true" {
                p.push(k.into());
            }
        }
        if self.attr("enabled") == "false" {
            p.push("disabled".into());
        }
        p.join(" ")
    }
}
fn bounds(s: &str) -> Option<[i32; 4]> {
    let n: Vec<i32> = s
        .split(['[', ']', ','])
        .filter(|v| !v.is_empty())
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    if n.len() != 4 || n[2] <= n[0] || n[3] <= n[1] {
        return None;
    }
    Some([n[0], n[1], n[2] - n[0], n[3] - n[1]])
}
// Parse a complete hierarchy first; never publish a partial ref map after malformed XML.
pub fn parse(xml: &str, generation: u64) -> Result<(Vec<Element>, Value)> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<Value> = Vec::new();
    let mut roots = Vec::new();
    let mut elements = Vec::new();
    let mut hierarchy = false;
    loop {
        match reader
            .read_event()
            .map_err(|e| format!("invalid hierarchy: {e}"))?
        {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"node" => {
                let empty =
                    xml.as_bytes().get(reader.buffer_position() as usize - 2) == Some(&b'/');
                let mut attrs = HashMap::new();
                for a in e.attributes() {
                    let a = a.map_err(|e| e.to_string())?;
                    attrs.insert(
                        String::from_utf8(a.key.as_ref().to_vec()).map_err(|e| e.to_string())?,
                        a.decode_and_unescape_value(reader.decoder())
                            .map_err(|e| e.to_string())?
                            .into_owned(),
                    );
                }
                if let Some(rect) = attrs.get("bounds").and_then(|s| bounds(s)) {
                    let reference = format!("@e{generation}_{}", elements.len() + 1);
                    elements.push(Element {
                        attrs: attrs.clone(),
                        rect,
                        reference,
                    });
                }
                let mut node = json!(attrs);
                node["children"] = json!([]);
                if empty {
                    append_node(node, &mut stack, &mut roots);
                } else {
                    stack.push(node);
                }
            }
            Event::Start(e) if e.name().as_ref() == b"hierarchy" => hierarchy = true,
            Event::End(e) if e.name().as_ref() == b"node" => {
                let node = stack.pop().ok_or("unbalanced hierarchy")?;
                append_node(node, &mut stack, &mut roots);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !hierarchy || !stack.is_empty() || roots.is_empty() {
        return Err("No UI hierarchy; screen may be off/locked or UI access unavailable".into());
    }
    Ok((elements, json!({"hierarchy":{"node":roots}})))
}
fn append_node(node: Value, stack: &mut [Value], roots: &mut Vec<Value>) {
    if let Some(parent) = stack.last_mut() {
        parent["children"].as_array_mut().unwrap().push(node);
    } else {
        roots.push(node);
    }
}
pub fn format(elements: &[Element], mode: &str) -> String {
    if mode == "json" {
        return format!(
            "Found these elements on screen: {}",
            json!(elements.iter().map(Element::json).collect::<Vec<_>>())
        );
    }
    std::iter::once("One element per line: @ref Type text= label= name= value= id= at=x,y size=WxH [focused] [selected] [checked] [disabled]".into()).chain(elements.iter().map(Element::line)).collect::<Vec<_>>().join("\n")
}
pub fn resolve(elements: &[Element], args: &Value) -> Result<(i32, i32)> {
    if let Some(reference) = args["ref"].as_str() {
        let e = elements
            .iter()
            .find(|e| e.reference == reference)
            .ok_or("STALE_REF: re-observe mobile_list_elements_on_screen; no input sent")?;
        if e.attr("enabled") == "false" {
            return Err("element disabled; no input sent".into());
        }
        return Ok((e.rect[0] + e.rect[2] / 2, e.rect[1] + e.rect[3] / 2));
    }
    let x = args["x"]
        .as_f64()
        .ok_or("Either ref or both x and y required")?;
    let y = args["y"]
        .as_f64()
        .ok_or("Either ref or both x and y required")?;
    if !x.is_finite()
        || !y.is_finite()
        || x < 0.
        || y < 0.
        || x > i32::MAX as f64
        || y > i32::MAX as f64
    {
        return Err("invalid coordinates".into());
    }
    Ok((x.round() as i32, y.round() as i32))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hierarchy_refs_and_formats_are_snapshot_scoped() {
        let xml = r#"<hierarchy rotation="0"><node class="android.widget.EditText" text="Rust中文&amp;" focused="true" enabled="true" bounds="[1,2][101,42]"><node class="Button" text="OK" bounds="[4,5][14,25]"/></node></hierarchy>"#;
        let (a, raw) = parse(xml, 1).unwrap();
        let (b, _) = parse(xml, 2).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].text(), "Rust中文&");
        assert!(a[0].focused_editor());
        assert_eq!(raw["hierarchy"]["node"][0]["children"][0]["text"], "OK");
        assert_eq!(
            resolve(&a, &json!({"ref":a[0].reference})).unwrap(),
            (51, 22)
        );
        assert!(resolve(&b, &json!({"ref":a[0].reference})).is_err());
        assert!(format(&a, "text").contains("focused"));
        assert!(format(&a, "json").contains("coordinates"));
        assert!(parse("<hierarchy><node>", 3).is_err());
    }
}
