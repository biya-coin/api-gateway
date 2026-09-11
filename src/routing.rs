use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoBackend {
    State,
    Indexer,
}

pub const INDEXER_TYPES: &[&str] = &[
    "metaAndAssetCtxs",
    "allMids",
    "l2Book",
    "webData2",
    "candleSnapshot",
    "historicalOrders",
    "orderStatus",
    "userFills",
    "userFillsByTime",
    "userFunding",
    "fundingHistory",
    "userNonFundingLedgerUpdates",
];

pub const STATE_TYPES: &[&str] = &[
    "meta",
    "recentTrades",
    "clearinghouseState",
    "activeAssetData",
    "openOrders",
    "frontendOpenOrders",
    "userFees",
    "unifiedBalances",
    "accountNonces",
    "marketSnapshot",
];

pub fn info_backend(body: &[u8]) -> Option<InfoBackend> {
    // Derived serde structs also accept sequences; the wire contract requires an object.
    if body
        .iter()
        .find(|byte| !matches!(byte, b' ' | b'\r' | b'\n' | b'\t'))
        != Some(&b'{')
    {
        return None;
    }
    #[derive(Deserialize)]
    struct Envelope<'a> {
        #[serde(rename = "type", borrow)]
        kind: std::borrow::Cow<'a, str>,
    }
    let envelope: Envelope<'_> = serde_json::from_slice(body).ok()?;
    if INDEXER_TYPES.contains(&envelope.kind.as_ref()) {
        Some(InfoBackend::Indexer)
    } else if STATE_TYPES.contains(&envelope.kind.as_ref()) {
        Some(InfoBackend::State)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_types_have_one_owner() {
        for (types, backend) in [
            (INDEXER_TYPES, InfoBackend::Indexer),
            (STATE_TYPES, InfoBackend::State),
        ] {
            for kind in types {
                assert_eq!(
                    info_backend(format!(r#"{{"type":"{kind}"}}"#).as_bytes()),
                    Some(backend)
                );
            }
        }
        assert!(!STATE_TYPES.iter().any(|kind| INDEXER_TYPES.contains(kind)));
    }

    #[test]
    fn invalid_unknown_and_deferred_types_are_rejected() {
        for body in [
            "{}",
            "[]",
            r#"["meta"]"#,
            r#"["orderStatus"]"#,
            "null",
            "bad",
            r#"{"type":1}"#,
            r#"{"type":"exchangeStatus"}"#,
            r#"{"type":"userRateLimit"}"#,
            r#"{"type":"meta","type":"orderStatus"}"#,
        ] {
            assert_eq!(info_backend(body.as_bytes()), None, "{body}");
        }
    }
}
