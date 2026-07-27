//! P-VTEC (Primary Valid Time Event Code) parsing.
//!
//! Fixed-width NWS wire format, so the field offsets below are the spec, not
//! an implementation choice:
//!
//! ```text
//! /O.NEW.KDLH.SV.W.0087.260727T0700Z-260727T0800Z/
//!  │ │   │    │  │ │    └─ valid time range
//!  │ │   │    │  │ └────── event tracking number
//!  │ │   │    │  └──────── significance (W arning / A watch / Y advisory / S statement)
//!  │ │   │    └─────────── phenomenon (TO tornado, SV svr tstorm, FF flash flood, ...)
//!  │ │   └──────────────── issuing office
//!  │ └──────────────────── action
//!  └────────────────────── product class (O perational / T est / E xperimental)
//! ```

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtecAction {
    New,
    Con,
    Ext,
    Exa,
    Exb,
    Upg,
    Can,
    Exp,
    Cor,
    Rou,
}

impl VtecAction {
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "NEW" => VtecAction::New,
            "CON" => VtecAction::Con,
            "EXT" => VtecAction::Ext,
            "EXA" => VtecAction::Exa,
            "EXB" => VtecAction::Exb,
            "UPG" => VtecAction::Upg,
            "CAN" => VtecAction::Can,
            "EXP" => VtecAction::Exp,
            "COR" => VtecAction::Cor,
            "ROU" => VtecAction::Rou,
            other => bail!("unknown VTEC action {other:?}"),
        })
    }

    pub fn is_first_issuance(self) -> bool {
        matches!(self, VtecAction::New)
    }

    /// UPG means the event was superseded by a higher significance product,
    /// so the original must be retired to avoid a stale duplicate on screen.
    pub fn terminates_event(self) -> bool {
        matches!(self, VtecAction::Can | VtecAction::Exp | VtecAction::Upg)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VtecCode {
    pub product_class: char,
    pub action: VtecAction,
    pub office: String,
    pub phenomenon: String,
    pub significance: char,
    pub etn: u16,
}

impl VtecCode {
    pub fn parse(raw: &str) -> Result<Self> {
        let body = raw.trim().trim_start_matches('/').trim_end_matches('/');
        let fields: Vec<&str> = body.split('.').collect();
        if fields.len() < 6 {
            bail!("malformed VTEC {raw:?}: expected 6 dot-separated fields, got {}", fields.len());
        }

        let product_class = match fields[0].chars().next() {
            Some(c) if fields[0].len() == 1 => c,
            _ => bail!("malformed VTEC {raw:?}: product class must be one character"),
        };

        let office = fields[2];
        if office.is_empty() {
            bail!("malformed VTEC {raw:?}: empty office id");
        }

        let phenomenon = fields[3];
        if phenomenon.len() != 2 {
            bail!("malformed VTEC {raw:?}: phenomenon must be two characters");
        }

        let significance = match fields[4].chars().next() {
            Some(c) if fields[4].len() == 1 => c,
            _ => bail!("malformed VTEC {raw:?}: significance must be one character"),
        };

        let etn: u16 = fields[5]
            .parse()
            .map_err(|_| anyhow::anyhow!("malformed VTEC {raw:?}: bad event tracking number"))?;

        Ok(VtecCode {
            product_class,
            action: VtecAction::parse(fields[1])?,
            office: office.to_string(),
            phenomenon: phenomenon.to_string(),
            significance,
            etn,
        })
    }

    /// The lookup key used by the lethality allowlist, e.g. `"SV.W"`.
    pub fn phenomenon_significance(&self) -> String {
        format!("{}.{}", self.phenomenon, self.significance)
    }

    pub fn is_operational(&self) -> bool {
        self.product_class == 'O'
    }

    /// Identity for deduplication across repeated polls.
    pub fn event_key(&self) -> (String, String, u16) {
        (self.office.clone(), self.phenomenon.clone(), self.etn)
    }

    /// NWS products may carry several VTEC strings, including H-VTEC for
    /// hydrologic events, which is not P-VTEC and must not be parsed as such.
    pub fn parse_all(raws: &[String]) -> Vec<VtecCode> {
        raws.iter()
            .flat_map(|r| r.split('\n'))
            .filter_map(|line| VtecCode::parse(line).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a5_parses_the_live_sample_from_api_weather_gov() {
        let v = VtecCode::parse("/O.NEW.KDLH.SV.W.0087.260727T0700Z-260727T0800Z/").unwrap();
        assert_eq!(v.product_class, 'O');
        assert_eq!(v.action, VtecAction::New);
        assert_eq!(v.office, "KDLH");
        assert_eq!(v.phenomenon, "SV");
        assert_eq!(v.significance, 'W');
        assert_eq!(v.etn, 87);
        assert_eq!(v.phenomenon_significance(), "SV.W");
        assert!(v.is_operational());
    }

    #[test]
    fn parses_the_second_live_sample() {
        let v = VtecCode::parse("/O.NEW.KAKQ.SV.W.0201.260727T0729Z-260727T0815Z/").unwrap();
        assert_eq!(v.office, "KAKQ");
        assert_eq!(v.etn, 201);
        assert_eq!(v.phenomenon_significance(), "SV.W");
    }

    #[test]
    fn tornado_warning_key_is_distinguishable() {
        let v = VtecCode::parse("/O.NEW.KTLX.TO.W.0012.260727T0700Z-260727T0730Z/").unwrap();
        assert_eq!(v.phenomenon_significance(), "TO.W");
        assert!(v.action.is_first_issuance());
    }

    #[test]
    fn continuation_is_not_a_first_issuance() {
        let v = VtecCode::parse("/O.CON.KTLX.TO.W.0012.260727T0700Z-260727T0730Z/").unwrap();
        assert!(!v.action.is_first_issuance());
        assert!(!v.action.terminates_event());
    }

    #[test]
    fn cancel_expire_and_upgrade_all_terminate() {
        for action in ["CAN", "EXP", "UPG"] {
            let raw = format!("/O.{action}.KTLX.TO.W.0012.260727T0700Z-260727T0730Z/");
            assert!(
                VtecCode::parse(&raw).unwrap().action.terminates_event(),
                "{action} should terminate"
            );
        }
    }

    #[test]
    fn event_key_is_stable_across_actions() {
        let new = VtecCode::parse("/O.NEW.KTLX.TO.W.0012.260727T0700Z-260727T0730Z/").unwrap();
        let con = VtecCode::parse("/O.CON.KTLX.TO.W.0012.260727T0705Z-260727T0730Z/").unwrap();
        assert_eq!(new.event_key(), con.event_key());
    }

    #[test]
    fn different_etn_is_a_different_event() {
        let a = VtecCode::parse("/O.NEW.KTLX.TO.W.0012.260727T0700Z-260727T0730Z/").unwrap();
        let b = VtecCode::parse("/O.NEW.KTLX.TO.W.0013.260727T0700Z-260727T0730Z/").unwrap();
        assert_ne!(a.event_key(), b.event_key());
    }

    #[test]
    fn malformed_input_errors_and_never_panics() {
        for bad in [
            "",
            "/",
            "garbage",
            "/O.NEW.KDLH/",
            "/O.XXX.KDLH.SV.W.0087.260727T0700Z-260727T0800Z/",
            "/O.NEW.KDLH.SVR.W.0087.260727T0700Z-260727T0800Z/",
            "/O.NEW.KDLH.SV.W.notanumber.260727T0700Z-260727T0800Z/",
            "/O.NEW..SV.W.0087.260727T0700Z-260727T0800Z/",
        ] {
            assert!(VtecCode::parse(bad).is_err(), "expected Err for {bad:?}");
        }
    }

    #[test]
    fn parse_all_skips_unparseable_lines_such_as_h_vtec() {
        let raws = vec![
            "/O.NEW.KDLH.SV.W.0087.260727T0700Z-260727T0800Z/".to_string(),
            "/00000000T0000Z-000000T0000Z/00/NN/0.0/".to_string(),
        ];
        let parsed = VtecCode::parse_all(&raws);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].phenomenon_significance(), "SV.W");
    }
}
