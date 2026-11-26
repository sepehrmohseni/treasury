//! JetBrains DataGrip and IDE data sources: `dataSources.xml`, with user names from
//! `dataSources.local.xml` beside it. Passwords live in the system keychain and are not imported.

use crate::{Draft, Imported, Report, Skipped, apply_uri, unreadable};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::HashMap;
use std::path::Path;
use treasury_core::Result;
use treasury_core::config::{Engine, SecretSlot};

#[derive(Default, Clone)]
struct Source {
    name: String,
    uuid: String,
    driver: String,
    url: String,
    user: String,
}

/// The `<data-source>` elements of a dataSources file.
fn parse(xml: &str) -> Result<Vec<Source>> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut cur: Option<Source> = None;
    let mut field: Option<&'static str> = None;
    loop {
        match reader.read_event().map_err(|e| unreadable("DataGrip", e))? {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == "data-source" => {
                let mut s = Source::default();
                for a in e.attributes().flatten() {
                    let v = a
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map(|v| v.into_owned())
                        .unwrap_or_default();
                    match a.key.as_ref() {
                        "name" => s.name = v,
                        "uuid" => s.uuid = v,
                        _ => {}
                    }
                }
                cur = Some(s);
            }
            Event::Start(e) => {
                field = match e.name().as_ref() {
                    "driver-ref" => Some("driver"),
                    "jdbc-url" => Some("url"),
                    "user-name" => Some("user"),
                    _ => None,
                };
            }
            Event::Text(t) => {
                if let (Some(s), Some(f)) = (cur.as_mut(), field) {
                    append(s, f, &t);
                }
            }
            Event::GeneralRef(r) => {
                if let (Some(s), Some(f)) = (cur.as_mut(), field)
                    && let Some(text) = entity(&r)
                {
                    append(s, f, &text);
                }
            }
            Event::End(e) => {
                if e.name().as_ref() == "data-source"
                    && let Some(s) = cur.take()
                {
                    out.push(s);
                }
                field = None;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn append(s: &mut Source, field: &str, text: &str) {
    match field {
        "driver" => s.driver.push_str(text),
        "url" => s.url.push_str(text),
        _ => s.user.push_str(text),
    }
}

/// The text of an entity reference: the five predefined ones and numeric references.
fn entity(name: &str) -> Option<String> {
    Some(match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        n => {
            let code = if let Some(hex) = n.strip_prefix("#x").or_else(|| n.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                n.strip_prefix('#')?.parse().ok()?
            };
            char::from_u32(code)?.to_string()
        }
    })
}

fn engine(driver: &str, url: &str) -> Option<Engine> {
    let d = format!(
        "{} {}",
        driver.to_ascii_lowercase(),
        url.to_ascii_lowercase()
    );
    Some(if d.contains("mariadb") {
        Engine::MariaDb
    } else if d.contains("mysql") {
        Engine::MySql
    } else if d.contains("postgres") {
        Engine::Postgres
    } else if d.contains("sqlserver") || d.contains("azure") {
        Engine::SqlServer
    } else if d.contains("oracle") {
        Engine::Oracle
    } else if d.contains("db2") {
        Engine::Db2
    } else if d.contains("mongo") {
        Engine::MongoDb
    } else if d.contains("redis") {
        Engine::Redis
    } else if d.contains("cassandra") {
        Engine::Cassandra
    } else if d.contains("snowflake") {
        Engine::Snowflake
    } else if d.contains("databricks") {
        Engine::Databricks
    } else {
        return None;
    })
}

pub fn import(path: &Path) -> Result<Report> {
    let file = if path.is_dir() {
        let direct = path.join("dataSources.xml");
        if direct.exists() {
            direct
        } else {
            path.join(".idea").join("dataSources.xml")
        }
    } else {
        path.to_owned()
    };
    let xml = std::fs::read_to_string(&file).map_err(|e| unreadable("DataGrip", e))?;
    let sources = parse(&xml)?;
    let local: HashMap<String, Source> = file
        .parent()
        .map(|d| d.join("dataSources.local.xml"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|x| parse(&x).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.uuid.clone(), s))
        .collect();
    let mut report = Report::default();
    for s in sources {
        let Some(engine) = engine(&s.driver, &s.url) else {
            report.skipped.push(Skipped {
                name: s.name.clone(),
                reason: format!("Treasury does not support the \"{}\" driver.", s.driver),
            });
            continue;
        };
        let mut config = Draft::new(&s.name, engine).build()?;
        let mut secrets = Vec::new();
        if !s.url.is_empty()
            && let Some(pw) = apply_uri(&mut config, &s.url)
        {
            secrets.push((SecretSlot::Password, pw));
        }
        if let Some(l) = local.get(&s.uuid).filter(|l| !l.user.is_empty()) {
            config.user = Some(l.user.clone());
        }
        report.imported.push(Imported {
            config,
            secrets,
            notes: vec!["The password is in the system keychain of the other application; enter it before connecting.".into()],
        });
    }
    Ok(report)
}
