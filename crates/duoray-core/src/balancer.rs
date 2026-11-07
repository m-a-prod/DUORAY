//! Where the traffic actually goes: the node a balancer picked right now and
//! the country the connection leaves from.
//!
//! The node comes from xray's own API (`xray api bi`), enabled on loopback for
//! configs with a balancer. The exit country comes from Cloudflare's trace
//! page fetched through the tunnel: `loc=EE`, whatever the panel calls it.

use std::net::SocketAddr;

use serde_json::{Value, json};

use crate::server::{Server, Source};

/// The balancer that carries the traffic and the outbounds it picks from.
#[derive(Debug, Clone, PartialEq)]
pub struct Balancer {
    pub tag: String,
    /// Outbound tags its selector matches, in config order.
    pub members: Vec<String>,
}

/// The balancer a catch-all rule points at (one not limited to an inbound,
/// like the DNS one), else the first balancer. `None` without balancers.
pub fn main_balancer(config: &Value) -> Option<Balancer> {
    let balancers = config["routing"]["balancers"].as_array().filter(|b| !b.is_empty())?;
    let rules = config["routing"]["rules"].as_array().map(Vec::as_slice).unwrap_or_default();
    let tag = rules
        .iter()
        .filter(|r| r.get("inboundTag").is_none())
        .find_map(|r| r["balancerTag"].as_str())
        .or_else(|| balancers[0]["tag"].as_str())?
        .to_string();
    let balancer = balancers.iter().find(|b| b["tag"] == tag.as_str())?;
    let selector: Vec<&str> = balancer["selector"].as_array()?.iter().filter_map(Value::as_str).collect();
    let members = config["outbounds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o["tag"].as_str())
        // xray matches selector entries as tag prefixes.
        .filter(|t| selector.iter().any(|s| t.starts_with(s)))
        .map(String::from)
        .collect();
    Some(Balancer { tag, members })
}

/// Turns on xray's API on `addr` (loopback), replacing a panel's own API
/// section: we read which node a balancer picked (routing) and the traffic
/// through our SOCKS inbound (stats; the panel's other policy is kept).
pub fn add_api(config: &mut Value, addr: SocketAddr) {
    config["api"] =
        json!({ "tag": "duoray-api", "listen": addr.to_string(), "services": ["RoutingService", "StatsService"] });
    if !config["stats"].is_object() {
        config["stats"] = json!({});
    }
    if !config["policy"].is_object() {
        config["policy"] = json!({});
    }
    if !config["policy"]["system"].is_object() {
        config["policy"]["system"] = json!({});
    }
    config["policy"]["system"]["statsInboundUplink"] = json!(true);
    config["policy"]["system"]["statsInboundDownlink"] = json!(true);
}

/// Bytes through the SOCKS inbound `tag` in `xray api statsquery` output:
/// (uplink, downlink). Counters xray has not seen traffic on are left out.
pub fn parse_inbound_traffic(output: &str, tag: &str) -> Option<(u64, u64)> {
    let json: Value = serde_json::from_str(output).ok()?;
    let value = |dir: &str| {
        let name = format!("inbound>>>{tag}>>>traffic>>>{dir}");
        json["stat"].as_array()?.iter().find(|s| s["name"] == name.as_str()).map(|s| s["value"].as_u64().unwrap_or(0))
    };
    let (up, down) = (value("uplink"), value("downlink"));
    (up.is_some() || down.is_some()).then(|| (up.unwrap_or(0), down.unwrap_or(0)))
}

/// The node in `xray api bi <tag>` output: a manual override if set, else
/// the first of "Selects" (leastLoad / leastPing rank the best first).
pub fn parse_selected(output: &str) -> Option<String> {
    let mut section = "";
    let (mut overridden, mut first) = (None, None);
    for line in output.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("- ") {
            section = name.trim_end_matches(':');
            continue;
        }
        // "1   proxy-7": a rank, then the tag (absent when nothing is set).
        let mut cols = line.split_whitespace();
        let (Some(_rank), Some(tag)) = (cols.next(), cols.next()) else { continue };
        match section {
            "Selecting Override" => overridden = overridden.or(Some(tag.to_string())),
            "Selects" => first = first.or(Some(tag.to_string())),
            _ => {}
        }
    }
    overridden.or(first)
}

/// What Cloudflare's trace page says about the connection.
#[derive(Debug, Clone, PartialEq)]
pub struct Trace {
    /// ISO country code, upper case ("EE").
    pub loc: String,
    pub ip: String,
}

pub fn parse_trace(body: &str) -> Option<Trace> {
    let field = |key: &str| {
        body.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=')).map(str::trim).filter(|v| !v.is_empty())
    };
    let loc = field("loc")?.to_ascii_uppercase();
    (loc.len() == 2 && loc.bytes().all(|b| b.is_ascii_alphabetic())).then(|| Trace { loc, ip: field("ip").unwrap_or_default().to_string() })
}

/// Names for a balancer's nodes: an outbound identical (protocol, settings,
/// transport) to a single server of the same subscription takes its name.
/// Bridges are usually all the same entry and stay unnamed.
pub fn node_names(config: &Value, siblings: &[Server]) -> Vec<(String, String)> {
    let shape = |o: &Value| {
        let mut stream = o["streamSettings"].clone();
        if let Some(s) = stream.as_object_mut() {
            s.remove("sockopt");
        }
        (o["protocol"].clone(), o["settings"].clone(), stream)
    };
    let named: Vec<(_, &str)> = siblings
        .iter()
        .filter(|s| s.proxies == 1)
        .filter_map(|s| {
            let config = match &s.source {
                Source::Json(v) => v.clone(),
                Source::Link(p) => crate::xray::config_from_profile(p),
            };
            let proxy = config["outbounds"].as_array()?.iter().find(|o| is_proxy(o))?.clone();
            Some((shape(&proxy), s.name.as_str()))
        })
        .collect();
    config["outbounds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|o| is_proxy(o))
        .filter_map(|o| {
            let tag = o["tag"].as_str()?;
            let me = shape(o);
            let (_, name) = named.iter().find(|(s, _)| *s == me)?;
            Some((tag.to_string(), name.to_string()))
        })
        .collect()
}

fn is_proxy(o: &Value) -> bool {
    !matches!(o["protocol"].as_str(), Some("freedom" | "blackhole" | "dns" | "loopback") | None)
}

/// Name of a country by its ISO 3166-1 code (all of them, plus Kosovo's
/// XK), in the interface language; `None` for an unknown code, which is then
/// shown as is.
pub fn country_name(code: &str) -> Option<&'static str> {
    let table = if crate::i18n::english() { COUNTRIES_EN } else { COUNTRIES };
    table.iter().find(|(c, _)| c.eq_ignore_ascii_case(code)).map(|(_, n)| *n)
}

/// English names: iso-codes' common names, a few official ones shortened.
const COUNTRIES_EN: &[(&str, &str)] = &[
    ("AD", "Andorra"), ("AE", "United Arab Emirates"), ("AF", "Afghanistan"), ("AG", "Antigua and Barbuda"),
    ("AI", "Anguilla"), ("AL", "Albania"), ("AM", "Armenia"), ("AO", "Angola"), ("AQ", "Antarctica"),
    ("AR", "Argentina"), ("AS", "American Samoa"), ("AT", "Austria"), ("AU", "Australia"), ("AW", "Aruba"),
    ("AX", "Åland Islands"), ("AZ", "Azerbaijan"), ("BA", "Bosnia and Herzegovina"), ("BB", "Barbados"),
    ("BD", "Bangladesh"), ("BE", "Belgium"), ("BF", "Burkina Faso"), ("BG", "Bulgaria"), ("BH", "Bahrain"),
    ("BI", "Burundi"), ("BJ", "Benin"), ("BL", "Saint Barthélemy"), ("BM", "Bermuda"), ("BN", "Brunei Darussalam"),
    ("BO", "Bolivia"), ("BQ", "Bonaire, Sint Eustatius and Saba"), ("BR", "Brazil"), ("BS", "Bahamas"),
    ("BT", "Bhutan"), ("BV", "Bouvet Island"), ("BW", "Botswana"), ("BY", "Belarus"), ("BZ", "Belize"),
    ("CA", "Canada"), ("CC", "Cocos Islands"), ("CD", "DR Congo"), ("CF", "Central African Republic"),
    ("CG", "Congo"), ("CH", "Switzerland"), ("CI", "Côte d'Ivoire"), ("CK", "Cook Islands"), ("CL", "Chile"),
    ("CM", "Cameroon"), ("CN", "China"), ("CO", "Colombia"), ("CR", "Costa Rica"), ("CU", "Cuba"),
    ("CV", "Cabo Verde"), ("CW", "Curaçao"), ("CX", "Christmas Island"), ("CY", "Cyprus"), ("CZ", "Czechia"),
    ("DE", "Germany"), ("DJ", "Djibouti"), ("DK", "Denmark"), ("DM", "Dominica"), ("DO", "Dominican Republic"),
    ("DZ", "Algeria"), ("EC", "Ecuador"), ("EE", "Estonia"), ("EG", "Egypt"), ("EH", "Western Sahara"),
    ("ER", "Eritrea"), ("ES", "Spain"), ("ET", "Ethiopia"), ("FI", "Finland"), ("FJ", "Fiji"),
    ("FK", "Falkland Islands"), ("FM", "Micronesia"), ("FO", "Faroe Islands"), ("FR", "France"), ("GA", "Gabon"),
    ("GB", "United Kingdom"), ("GD", "Grenada"), ("GE", "Georgia"), ("GF", "French Guiana"), ("GG", "Guernsey"),
    ("GH", "Ghana"), ("GI", "Gibraltar"), ("GL", "Greenland"), ("GM", "Gambia"), ("GN", "Guinea"),
    ("GP", "Guadeloupe"), ("GQ", "Equatorial Guinea"), ("GR", "Greece"),
    ("GS", "South Georgia and the South Sandwich Islands"), ("GT", "Guatemala"), ("GU", "Guam"),
    ("GW", "Guinea-Bissau"), ("GY", "Guyana"), ("HK", "Hong Kong"), ("HM", "Heard Island and McDonald Islands"),
    ("HN", "Honduras"), ("HR", "Croatia"), ("HT", "Haiti"), ("HU", "Hungary"), ("ID", "Indonesia"), ("IE", "Ireland"),
    ("IL", "Israel"), ("IM", "Isle of Man"), ("IN", "India"), ("IO", "British Indian Ocean Territory"),
    ("IQ", "Iraq"), ("IR", "Iran"), ("IS", "Iceland"), ("IT", "Italy"), ("JE", "Jersey"), ("JM", "Jamaica"),
    ("JO", "Jordan"), ("JP", "Japan"), ("KE", "Kenya"), ("KG", "Kyrgyzstan"), ("KH", "Cambodia"), ("KI", "Kiribati"),
    ("KM", "Comoros"), ("KN", "Saint Kitts and Nevis"), ("KP", "North Korea"), ("KR", "South Korea"),
    ("KW", "Kuwait"), ("KY", "Cayman Islands"), ("KZ", "Kazakhstan"), ("LA", "Laos"), ("LB", "Lebanon"),
    ("LC", "Saint Lucia"), ("LI", "Liechtenstein"), ("LK", "Sri Lanka"), ("LR", "Liberia"), ("LS", "Lesotho"),
    ("LT", "Lithuania"), ("LU", "Luxembourg"), ("LV", "Latvia"), ("LY", "Libya"), ("MA", "Morocco"), ("MC", "Monaco"),
    ("MD", "Moldova"), ("ME", "Montenegro"), ("MF", "Saint Martin"), ("MG", "Madagascar"), ("MH", "Marshall Islands"),
    ("MK", "North Macedonia"), ("ML", "Mali"), ("MM", "Myanmar"), ("MN", "Mongolia"), ("MO", "Macao"),
    ("MP", "Northern Mariana Islands"), ("MQ", "Martinique"), ("MR", "Mauritania"), ("MS", "Montserrat"),
    ("MT", "Malta"), ("MU", "Mauritius"), ("MV", "Maldives"), ("MW", "Malawi"), ("MX", "Mexico"), ("MY", "Malaysia"),
    ("MZ", "Mozambique"), ("NA", "Namibia"), ("NC", "New Caledonia"), ("NE", "Niger"), ("NF", "Norfolk Island"),
    ("NG", "Nigeria"), ("NI", "Nicaragua"), ("NL", "Netherlands"), ("NO", "Norway"), ("NP", "Nepal"), ("NR", "Nauru"),
    ("NU", "Niue"), ("NZ", "New Zealand"), ("OM", "Oman"), ("PA", "Panama"), ("PE", "Peru"),
    ("PF", "French Polynesia"), ("PG", "Papua New Guinea"), ("PH", "Philippines"), ("PK", "Pakistan"),
    ("PL", "Poland"), ("PM", "Saint Pierre and Miquelon"), ("PN", "Pitcairn"), ("PR", "Puerto Rico"),
    ("PS", "Palestine"), ("PT", "Portugal"), ("PW", "Palau"), ("PY", "Paraguay"), ("QA", "Qatar"), ("RE", "Réunion"),
    ("RO", "Romania"), ("RS", "Serbia"), ("RU", "Russia"), ("RW", "Rwanda"), ("SA", "Saudi Arabia"),
    ("SB", "Solomon Islands"), ("SC", "Seychelles"), ("SD", "Sudan"), ("SE", "Sweden"), ("SG", "Singapore"),
    ("SH", "Saint Helena"), ("SI", "Slovenia"), ("SJ", "Svalbard and Jan Mayen"), ("SK", "Slovakia"),
    ("SL", "Sierra Leone"), ("SM", "San Marino"), ("SN", "Senegal"), ("SO", "Somalia"), ("SR", "Suriname"),
    ("SS", "South Sudan"), ("ST", "Sao Tome and Principe"), ("SV", "El Salvador"), ("SX", "Sint Maarten"),
    ("SY", "Syria"), ("SZ", "Eswatini"), ("TC", "Turks and Caicos Islands"), ("TD", "Chad"),
    ("TF", "French Southern Territories"), ("TG", "Togo"), ("TH", "Thailand"), ("TJ", "Tajikistan"),
    ("TK", "Tokelau"), ("TL", "Timor-Leste"), ("TM", "Turkmenistan"), ("TN", "Tunisia"), ("TO", "Tonga"),
    ("TR", "Türkiye"), ("TT", "Trinidad and Tobago"), ("TV", "Tuvalu"), ("TW", "Taiwan"), ("TZ", "Tanzania"),
    ("UA", "Ukraine"), ("UG", "Uganda"), ("UM", "United States Minor Outlying Islands"), ("US", "United States"),
    ("UY", "Uruguay"), ("UZ", "Uzbekistan"), ("VA", "Vatican City"), ("VC", "Saint Vincent and the Grenadines"),
    ("VE", "Venezuela"), ("VG", "British Virgin Islands"), ("VI", "U.S. Virgin Islands"), ("VN", "Vietnam"),
    ("VU", "Vanuatu"), ("WF", "Wallis and Futuna"), ("WS", "Samoa"), ("XK", "Kosovo"), ("YE", "Yemen"),
    ("YT", "Mayotte"), ("ZA", "South Africa"), ("ZM", "Zambia"), ("ZW", "Zimbabwe"),
];


const COUNTRIES: &[(&str, &str)] = &[
    ("AD", "Андорра"), ("AE", "ОАЭ"), ("AF", "Афганистан"), ("AG", "Антигуа и Барбуда"), ("AI", "Ангилья"),
    ("AL", "Албания"), ("AM", "Армения"), ("AO", "Ангола"), ("AQ", "Антарктида"), ("AR", "Аргентина"),
    ("AS", "Американское Самоа"), ("AT", "Австрия"), ("AU", "Австралия"), ("AW", "Аруба"),
    ("AX", "Аландские острова"), ("AZ", "Азербайджан"), ("BA", "Босния и Герцеговина"), ("BB", "Барбадос"),
    ("BD", "Бангладеш"), ("BE", "Бельгия"), ("BF", "Буркина-Фасо"), ("BG", "Болгария"), ("BH", "Бахрейн"),
    ("BI", "Бурунди"), ("BJ", "Бенин"), ("BL", "Сен-Бартелеми"), ("BM", "Бермуды"), ("BN", "Бруней"),
    ("BO", "Боливия"), ("BQ", "Бонайре, Синт-Эстатиус и Саба"), ("BR", "Бразилия"), ("BS", "Багамы"),
    ("BT", "Бутан"), ("BV", "Остров Буве"), ("BW", "Ботсвана"), ("BY", "Беларусь"), ("BZ", "Белиз"),
    ("CA", "Канада"), ("CC", "Кокосовые острова"), ("CD", "ДР Конго"), ("CF", "ЦАР"), ("CG", "Республика Конго"),
    ("CH", "Швейцария"), ("CI", "Кот-д’Ивуар"), ("CK", "Острова Кука"), ("CL", "Чили"), ("CM", "Камерун"),
    ("CN", "Китай"), ("CO", "Колумбия"), ("CR", "Коста-Рика"), ("CU", "Куба"), ("CV", "Кабо-Верде"),
    ("CW", "Кюрасао"), ("CX", "Остров Рождества"), ("CY", "Кипр"), ("CZ", "Чехия"), ("DE", "Германия"),
    ("DJ", "Джибути"), ("DK", "Дания"), ("DM", "Доминика"), ("DO", "Доминиканская Республика"), ("DZ", "Алжир"),
    ("EC", "Эквадор"), ("EE", "Эстония"), ("EG", "Египет"), ("EH", "Западная Сахара"), ("ER", "Эритрея"),
    ("ES", "Испания"), ("ET", "Эфиопия"), ("FI", "Финляндия"), ("FJ", "Фиджи"), ("FK", "Фолклендские острова"),
    ("FM", "Микронезия"), ("FO", "Фарерские острова"), ("FR", "Франция"), ("GA", "Габон"), ("GB", "Великобритания"),
    ("GD", "Гренада"), ("GE", "Грузия"), ("GF", "Французская Гвиана"), ("GG", "Гернси"), ("GH", "Гана"),
    ("GI", "Гибралтар"), ("GL", "Гренландия"), ("GM", "Гамбия"), ("GN", "Гвинея"), ("GP", "Гваделупа"),
    ("GQ", "Экваториальная Гвинея"), ("GR", "Греция"), ("GS", "Южная Георгия и Южные Сандвичевы острова"),
    ("GT", "Гватемала"), ("GU", "Гуам"), ("GW", "Гвинея-Бисау"), ("GY", "Гайана"), ("HK", "Гонконг"),
    ("HM", "Острова Херд и Макдональд"), ("HN", "Гондурас"), ("HR", "Хорватия"), ("HT", "Гаити"), ("HU", "Венгрия"),
    ("ID", "Индонезия"), ("IE", "Ирландия"), ("IL", "Израиль"), ("IM", "Остров Мэн"), ("IN", "Индия"),
    ("IO", "Британская территория в Индийском океане"), ("IQ", "Ирак"), ("IR", "Иран"), ("IS", "Исландия"),
    ("IT", "Италия"), ("JE", "Джерси"), ("JM", "Ямайка"), ("JO", "Иордания"), ("JP", "Япония"), ("KE", "Кения"),
    ("KG", "Киргизия"), ("KH", "Камбоджа"), ("KI", "Кирибати"), ("KM", "Коморы"), ("KN", "Сент-Китс и Невис"),
    ("KP", "КНДР"), ("KR", "Южная Корея"), ("KW", "Кувейт"), ("KY", "Каймановы острова"), ("KZ", "Казахстан"),
    ("LA", "Лаос"), ("LB", "Ливан"), ("LC", "Сент-Люсия"), ("LI", "Лихтенштейн"), ("LK", "Шри-Ланка"),
    ("LR", "Либерия"), ("LS", "Лесото"), ("LT", "Литва"), ("LU", "Люксембург"), ("LV", "Латвия"), ("LY", "Ливия"),
    ("MA", "Марокко"), ("MC", "Монако"), ("MD", "Молдова"), ("ME", "Черногория"), ("MF", "Сен-Мартен"),
    ("MG", "Мадагаскар"), ("MH", "Маршалловы Острова"), ("MK", "Северная Македония"), ("ML", "Мали"),
    ("MM", "Мьянма"), ("MN", "Монголия"), ("MO", "Макао"), ("MP", "Северные Марианские острова"),
    ("MQ", "Мартиника"), ("MR", "Мавритания"), ("MS", "Монтсеррат"), ("MT", "Мальта"), ("MU", "Маврикий"),
    ("MV", "Мальдивы"), ("MW", "Малави"), ("MX", "Мексика"), ("MY", "Малайзия"), ("MZ", "Мозамбик"),
    ("NA", "Намибия"), ("NC", "Новая Каледония"), ("NE", "Нигер"), ("NF", "Остров Норфолк"), ("NG", "Нигерия"),
    ("NI", "Никарагуа"), ("NL", "Нидерланды"), ("NO", "Норвегия"), ("NP", "Непал"), ("NR", "Науру"), ("NU", "Ниуэ"),
    ("NZ", "Новая Зеландия"), ("OM", "Оман"), ("PA", "Панама"), ("PE", "Перу"), ("PF", "Французская Полинезия"),
    ("PG", "Папуа — Новая Гвинея"), ("PH", "Филиппины"), ("PK", "Пакистан"), ("PL", "Польша"),
    ("PM", "Сен-Пьер и Микелон"), ("PN", "Острова Питкэрн"), ("PR", "Пуэрто-Рико"), ("PS", "Палестина"),
    ("PT", "Португалия"), ("PW", "Палау"), ("PY", "Парагвай"), ("QA", "Катар"), ("RE", "Реюньон"),
    ("RO", "Румыния"), ("RS", "Сербия"), ("RU", "Россия"), ("RW", "Руанда"), ("SA", "Саудовская Аравия"),
    ("SB", "Соломоновы Острова"), ("SC", "Сейшелы"), ("SD", "Судан"), ("SE", "Швеция"), ("SG", "Сингапур"),
    ("SH", "Остров Святой Елены"), ("SI", "Словения"), ("SJ", "Шпицберген и Ян-Майен"), ("SK", "Словакия"),
    ("SL", "Сьерра-Леоне"), ("SM", "Сан-Марино"), ("SN", "Сенегал"), ("SO", "Сомали"), ("SR", "Суринам"),
    ("SS", "Южный Судан"), ("ST", "Сан-Томе и Принсипи"), ("SV", "Сальвадор"), ("SX", "Синт-Мартен"),
    ("SY", "Сирия"), ("SZ", "Эсватини"), ("TC", "Теркс и Кайкос"), ("TD", "Чад"),
    ("TF", "Французские Южные территории"), ("TG", "Того"), ("TH", "Таиланд"), ("TJ", "Таджикистан"),
    ("TK", "Токелау"), ("TL", "Восточный Тимор"), ("TM", "Туркменистан"), ("TN", "Тунис"), ("TO", "Тонга"),
    ("TR", "Турция"), ("TT", "Тринидад и Тобаго"), ("TV", "Тувалу"), ("TW", "Тайвань"), ("TZ", "Танзания"),
    ("UA", "Украина"), ("UG", "Уганда"), ("UM", "Внешние малые острова США"), ("US", "США"), ("UY", "Уругвай"),
    ("UZ", "Узбекистан"), ("VA", "Ватикан"), ("VC", "Сент-Винсент и Гренадины"), ("VE", "Венесуэла"),
    ("VG", "Британские Виргинские острова"), ("VI", "Виргинские острова США"), ("VN", "Вьетнам"), ("VU", "Вануату"),
    ("WF", "Уоллис и Футуна"), ("WS", "Самоа"), ("XK", "Косово"), ("YE", "Йемен"), ("YT", "Майотта"), ("ZA", "ЮАР"),
    ("ZM", "Замбия"), ("ZW", "Зимбабве"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_catch_all_balancer_and_its_nodes() {
        let config = json!({
            "outbounds": [{"tag": "proxy"}, {"tag": "proxy-2"}, {"tag": "direct"}, {"tag": "lte-1"}],
            "routing": {
                "balancers": [{"tag": "dns-b", "selector": ["lte"]}, {"tag": "Super_Balancer", "selector": ["proxy"]}],
                "rules": [
                    {"inboundTag": ["dns-internal"], "balancerTag": "dns-b"},
                    {"network": "tcp,udp", "balancerTag": "Super_Balancer"}
                ]
            }
        });
        let b = main_balancer(&config).unwrap();
        assert_eq!(b.tag, "Super_Balancer");
        assert_eq!(b.members, ["proxy", "proxy-2"]);
        assert_eq!(main_balancer(&json!({"routing": {}})), None);
    }

    #[test]
    fn api_keeps_the_panels_policy() {
        let mut config = json!({"policy": {"levels": {"0": {"handshake": 4}}, "system": {"statsOutboundUplink": true}}});
        add_api(&mut config, "127.0.0.1:9".parse().unwrap());
        assert_eq!(config["api"]["services"], json!(["RoutingService", "StatsService"]));
        assert_eq!(config["policy"]["levels"]["0"]["handshake"], 4);
        assert_eq!(config["policy"]["system"], json!({"statsOutboundUplink": true, "statsInboundUplink": true, "statsInboundDownlink": true}));
        assert_eq!(config["stats"], json!({}));
    }

    #[test]
    fn reads_inbound_traffic() {
        let out = r#"{"stat": [{"name": "inbound>>>socks>>>traffic>>>uplink", "value": 135},
                               {"name": "inbound>>>socks>>>traffic>>>downlink"},
                               {"name": "outbound>>>proxy>>>traffic>>>downlink", "value": 9}]}"#;
        assert_eq!(parse_inbound_traffic(out, "socks"), Some((135, 0)));
        assert_eq!(parse_inbound_traffic(out, "other"), None);
        assert_eq!(parse_inbound_traffic("garbage", "socks"), None);
    }

    #[test]
    fn reads_the_selected_node() {
        let picked = "  - Selecting Override:\n    1                 \n  - Selects:\n    1   proxy-7       \n    2   proxy         \n";
        assert_eq!(parse_selected(picked).as_deref(), Some("proxy-7"));
        let forced = "  - Selecting Override:\n    1   proxy-3       \n  - Selects:\n    1   proxy-7       \n";
        assert_eq!(parse_selected(forced).as_deref(), Some("proxy-3"));
        assert_eq!(parse_selected("  - Selecting Override:\n    1                 \n  - Selects:\n"), None);
    }

    #[test]
    fn reads_the_exit_country() {
        let body = "fl=152f77\nh=1.1.1.1\nip=5.175.178.192\ncolo=TLL\nloc=EE\ntls=TLSv1.3\n";
        assert_eq!(parse_trace(body), Some(Trace { loc: "EE".into(), ip: "5.175.178.192".into() }));
        assert_eq!(parse_trace("ip=1.2.3.4\nloc=XX1\n"), None);
        assert_eq!(country_name("ee"), Some("Эстония"));
        assert_eq!(country_name("NG"), Some("Нигерия"));
        assert_eq!(country_name("ZZ"), None);
    }

    #[test]
    fn names_nodes_after_identical_single_servers() {
        let node = |addr: &str| json!({"protocol": "vless", "settings": {"vnext": [{"address": addr, "port": 443}]},
                                      "streamSettings": {"network": "tcp"}});
        let mut balanced = json!({"outbounds": [node("1.1.1.1"), node("2.2.2.2"), {"tag": "direct", "protocol": "freedom"}]});
        balanced["outbounds"][0]["tag"] = json!("proxy");
        balanced["outbounds"][1]["tag"] = json!("proxy-2");
        let mut single = json!({"outbounds": [node("2.2.2.2"), {"protocol": "freedom"}]});
        single["outbounds"][0]["tag"] = json!("proxy");
        single["outbounds"][0]["streamSettings"]["sockopt"] = json!({"mark": 1});
        let sibling = Server::from_xray_config(single).map(|mut s| {
            s.name = "Финляндия #2".into();
            s
        });
        let names = node_names(&balanced, &sibling.into_iter().collect::<Vec<_>>());
        assert_eq!(names, [("proxy-2".to_string(), "Финляндия #2".to_string())]);
    }
}
