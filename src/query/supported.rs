use serde::Serialize;

use super::ir::{
    Breakdown, EventMatch, FilterOperator, FilterSource, GroupOrFilter, Math, PropertyGroup, Query,
    QueryKind,
};

pub const MAX_SERIES: usize = 10;
pub const MAX_FILTERS: usize = 32;
pub const MAX_FILTER_DEPTH: usize = 2;
pub const MAX_BREAKDOWN_LIMIT: usize = 100;
pub const MAX_TEXT_BYTES: usize = 256;

/// A query whose complete shape is known to have implemented semantics.
///
/// The wrapped IR stays private so callers cannot invalidate the guarantee
/// after construction. Broad [`Query`] deserialization remains the wire and
/// saved-insight compatibility seam.
#[derive(Debug, Clone)]
pub struct SupportedQuery(Query);

/// The first unsupported part of a broadly-deserialized query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnsupportedQuery {
    pub code: &'static str,
    pub field: String,
    pub message: String,
}

impl UnsupportedQuery {
    fn at(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: "unsupported_query",
            field: field.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for UnsupportedQuery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} at {}: {}",
            self.code, self.field, self.message
        )
    }
}

impl std::error::Error for UnsupportedQuery {}

impl TryFrom<Query> for SupportedQuery {
    type Error = UnsupportedQuery;

    fn try_from(mut query: Query) -> Result<Self, Self::Error> {
        if !matches!(query.kind, QueryKind::Trends) {
            return Err(UnsupportedQuery::at(
                "kind",
                "only trends queries are supported",
            ));
        }

        if query.series.is_empty() {
            return Err(UnsupportedQuery::at(
                "series",
                "at least one series is required",
            ));
        }
        if query.series.len() > MAX_SERIES {
            return Err(UnsupportedQuery::at(
                "series",
                format!("at most {MAX_SERIES} series are supported"),
            ));
        }

        for (index, series) in query.series.iter().enumerate() {
            if let EventMatch::Name(name) = &series.event {
                if name.is_empty() {
                    return Err(UnsupportedQuery::at(
                        format!("series[{index}].event"),
                        "event name must not be empty",
                    ));
                }
                if name.len() > MAX_TEXT_BYTES {
                    return Err(UnsupportedQuery::at(
                        format!("series[{index}].event"),
                        format!("event name must be at most {MAX_TEXT_BYTES} bytes"),
                    ));
                }
            }
            if !matches!(series.math, Math::Total | Math::UniquePersons) {
                return Err(UnsupportedQuery::at(
                    format!("series[{index}].math"),
                    "only total and unique_persons math are supported",
                ));
            }
        }

        let mut filter_count = 0;
        validate_filter_group(&query.filters, "filters", 1, &mut filter_count)?;
        if let Some(breakdown) = &query.breakdown {
            validate_breakdown(breakdown)?;
        }
        if query.breakdown2.is_some() {
            return Err(UnsupportedQuery::at(
                "breakdown2",
                "only one breakdown is supported",
            ));
        }
        normalize_range(&mut query.range)?;
        if !query.formulas.is_empty() {
            return Err(UnsupportedQuery::at(
                "formulas",
                "formulas are not supported",
            ));
        }
        if query.funnel_config.is_some() {
            return Err(UnsupportedQuery::at(
                "funnel_config",
                "funnel configuration is not supported",
            ));
        }
        if query.retention_config.is_some() {
            return Err(UnsupportedQuery::at(
                "retention_config",
                "retention configuration is not supported",
            ));
        }
        if query.lifecycle_config.is_some() {
            return Err(UnsupportedQuery::at(
                "lifecycle_config",
                "lifecycle configuration is not supported",
            ));
        }
        if query.stickiness_config.is_some() {
            return Err(UnsupportedQuery::at(
                "stickiness_config",
                "stickiness configuration is not supported",
            ));
        }
        if query.actors_config.is_some() {
            return Err(UnsupportedQuery::at(
                "actors_config",
                "actors configuration is not supported",
            ));
        }
        if query.sql_config.is_some() {
            return Err(UnsupportedQuery::at(
                "sql_config",
                "SQL configuration is not supported",
            ));
        }

        Ok(Self(query))
    }
}

fn normalize_range(range: &mut super::ir::DateRange) -> Result<(), UnsupportedQuery> {
    if range.last_n.is_some() {
        return Err(UnsupportedQuery::at(
            "range.last_n",
            "relative date ranges are not supported",
        ));
    }
    let from = range.from.as_deref().ok_or_else(|| {
        UnsupportedQuery::at("range.from", "an absolute from timestamp is required")
    })?;
    let to = range
        .to
        .as_deref()
        .ok_or_else(|| UnsupportedQuery::at("range.to", "an absolute to timestamp is required"))?;
    let from = chrono::DateTime::parse_from_rfc3339(from)
        .map_err(|_| UnsupportedQuery::at("range.from", "from must be an RFC3339 timestamp"))?;
    let to = chrono::DateTime::parse_from_rfc3339(to)
        .map_err(|_| UnsupportedQuery::at("range.to", "to must be an RFC3339 timestamp"))?;
    if from >= to {
        return Err(UnsupportedQuery::at(
            "range",
            "from must be strictly before to",
        ));
    }
    range.from = Some(
        from.to_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
    );
    range.to = Some(
        to.to_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
    );

    Ok(())
}

fn validate_breakdown(breakdown: &Breakdown) -> Result<(), UnsupportedQuery> {
    if !matches!(breakdown.source, FilterSource::Event) {
        return Err(UnsupportedQuery::at(
            "breakdown.source",
            "only event property breakdowns are supported",
        ));
    }
    if breakdown.key.is_empty() {
        return Err(UnsupportedQuery::at(
            "breakdown.key",
            "breakdown key must not be empty",
        ));
    }
    if breakdown.key.len() > MAX_TEXT_BYTES {
        return Err(UnsupportedQuery::at(
            "breakdown.key",
            format!("breakdown key must be at most {MAX_TEXT_BYTES} bytes"),
        ));
    }
    if !(1..=MAX_BREAKDOWN_LIMIT).contains(&breakdown.limit) {
        return Err(UnsupportedQuery::at(
            "breakdown.limit",
            format!("breakdown limit must be between 1 and {MAX_BREAKDOWN_LIMIT}"),
        ));
    }

    Ok(())
}

fn validate_filter_group(
    group: &PropertyGroup,
    path: &str,
    depth: usize,
    filter_count: &mut usize,
) -> Result<(), UnsupportedQuery> {
    for (index, item) in group.values.iter().enumerate() {
        let item_path = format!("{path}.values[{index}]");
        match item {
            GroupOrFilter::Group(child) => {
                if depth >= MAX_FILTER_DEPTH {
                    return Err(UnsupportedQuery::at(
                        item_path,
                        format!("filter groups support at most {MAX_FILTER_DEPTH} levels"),
                    ));
                }
                validate_filter_group(child, &item_path, depth + 1, filter_count)?;
            }
            GroupOrFilter::Filter(filter) => {
                *filter_count += 1;
                if *filter_count > MAX_FILTERS {
                    return Err(UnsupportedQuery::at(
                        "filters",
                        format!("at most {MAX_FILTERS} filters are supported"),
                    ));
                }
                if !matches!(filter.source, FilterSource::Event) {
                    return Err(UnsupportedQuery::at(
                        format!("{item_path}.source"),
                        "only event property filters are supported",
                    ));
                }
                if filter.key.is_empty() {
                    return Err(UnsupportedQuery::at(
                        format!("{item_path}.key"),
                        "filter key must not be empty",
                    ));
                }
                if filter.key.len() > MAX_TEXT_BYTES {
                    return Err(UnsupportedQuery::at(
                        format!("{item_path}.key"),
                        format!("filter key must be at most {MAX_TEXT_BYTES} bytes"),
                    ));
                }
                if !matches!(
                    &filter.operator,
                    FilterOperator::Exact
                        | FilterOperator::IExact
                        | FilterOperator::NotEqual
                        | FilterOperator::Contains
                        | FilterOperator::NotContains
                        | FilterOperator::IContains
                        | FilterOperator::IsSet
                        | FilterOperator::IsNotSet
                ) {
                    return Err(UnsupportedQuery::at(
                        format!("{item_path}.operator"),
                        "filter operator is not supported",
                    ));
                }
                if matches!(
                    &filter.operator,
                    FilterOperator::Exact
                        | FilterOperator::IExact
                        | FilterOperator::NotEqual
                        | FilterOperator::Contains
                        | FilterOperator::NotContains
                        | FilterOperator::IContains
                ) && !filter.value.is_string()
                {
                    return Err(UnsupportedQuery::at(
                        format!("{item_path}.value"),
                        "filter value must be a string for this operator",
                    ));
                }
            }
        }
    }

    Ok(())
}

impl SupportedQuery {
    /// Borrows the compatible IR for compilation without exposing mutation.
    pub fn as_query(&self) -> &Query {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ir::{
        DateRange, Filter, FilterOperator, FilterSource, GroupOp, GroupOrFilter, Interval, LastN,
        PropertyGroup, Query, QueryKind, Series,
    };

    fn query() -> Query {
        Query::trends(
            vec![Series {
                event: crate::query::ir::EventMatch::Name("pageview".into()),
                math: crate::query::ir::Math::Total,
            }],
            DateRange {
                from: Some("2026-08-01T00:00:00Z".into()),
                to: Some("2026-08-02T00:00:00Z".into()),
                last_n: None,
            },
        )
    }

    fn error(raw: Query) -> UnsupportedQuery {
        SupportedQuery::try_from(raw).unwrap_err()
    }

    fn filter(operator: FilterOperator) -> GroupOrFilter {
        GroupOrFilter::Filter(Filter {
            source: FilterSource::Event,
            key: "browser".into(),
            operator,
            value: serde_json::json!("Firefox"),
        })
    }

    #[test]
    fn rejects_non_trends_kind_with_structured_error() {
        let mut raw = query();
        raw.kind = QueryKind::Funnels;

        let error = SupportedQuery::try_from(raw).unwrap_err();

        assert_eq!(error.code, "unsupported_query");
        assert_eq!(error.field, "kind");
        assert_eq!(error.message, "only trends queries are supported");
    }

    #[test]
    fn accepts_total_and_unique_persons_math() {
        let mut raw = query();
        raw.series.push(Series {
            event: crate::query::ir::EventMatch::Any,
            math: crate::query::ir::Math::UniquePersons,
        });

        let supported = SupportedQuery::try_from(raw).unwrap();

        assert_eq!(supported.as_query().series.len(), 2);
    }

    #[test]
    fn rejects_unimplemented_math_at_the_series_field() {
        let mut raw = query();
        raw.series[0].math = crate::query::ir::Math::Dau;

        let error = SupportedQuery::try_from(raw).unwrap_err();

        assert_eq!(error.field, "series[0].math");
        assert_eq!(
            error.message,
            "only total and unique_persons math are supported"
        );
    }

    #[test]
    fn enforces_bounded_named_series_in_stable_order() {
        let mut empty = query();
        empty.series.clear();
        assert_eq!(error(empty).field, "series");

        let mut too_many = query();
        too_many.series = (0..=MAX_SERIES)
            .map(|_| Series {
                event: crate::query::ir::EventMatch::Any,
                math: crate::query::ir::Math::Total,
            })
            .collect();
        assert_eq!(error(too_many).message, "at most 10 series are supported");

        let mut empty_name = query();
        empty_name.series[0].event = crate::query::ir::EventMatch::Name(String::new());
        assert_eq!(error(empty_name).field, "series[0].event");

        let mut long_name = query();
        long_name.series[0].event =
            crate::query::ir::EventMatch::Name("e".repeat(MAX_TEXT_BYTES + 1));
        long_name.series[0].math = crate::query::ir::Math::Dau;
        assert_eq!(error(long_name).field, "series[0].event");
    }

    #[test]
    fn accepts_only_bounded_event_filter_trees() {
        let allowed = [
            FilterOperator::Exact,
            FilterOperator::IExact,
            FilterOperator::NotEqual,
            FilterOperator::Contains,
            FilterOperator::NotContains,
            FilterOperator::IContains,
            FilterOperator::IsSet,
            FilterOperator::IsNotSet,
        ];
        let mut valid = query();
        valid.filters = PropertyGroup {
            op: GroupOp::And,
            values: vec![GroupOrFilter::Group(PropertyGroup {
                op: GroupOp::Or,
                values: allowed.into_iter().map(filter).collect(),
            })],
        };
        SupportedQuery::try_from(valid).unwrap();

        let mut person = query();
        person.filters.values.push(filter(FilterOperator::Exact));
        let GroupOrFilter::Filter(person_filter) = &mut person.filters.values[0] else {
            unreachable!()
        };
        person_filter.source = FilterSource::Person;
        assert_eq!(error(person).field, "filters.values[0].source");

        let mut numeric = query();
        numeric.filters.values.push(filter(FilterOperator::Gt));
        assert_eq!(error(numeric).field, "filters.values[0].operator");

        let mut too_deep = query();
        too_deep
            .filters
            .values
            .push(GroupOrFilter::Group(PropertyGroup {
                op: GroupOp::And,
                values: vec![GroupOrFilter::Group(PropertyGroup {
                    op: GroupOp::And,
                    values: vec![filter(FilterOperator::Exact)],
                })],
            }));
        assert_eq!(error(too_deep).field, "filters.values[0].values[0]");

        let mut too_many = query();
        too_many.filters.values = (0..=MAX_FILTERS)
            .map(|_| filter(FilterOperator::Exact))
            .collect();
        assert_eq!(error(too_many).field, "filters");
    }

    #[test]
    fn string_comparison_filters_require_json_string_values() {
        let operators = [
            FilterOperator::Exact,
            FilterOperator::IExact,
            FilterOperator::NotEqual,
            FilterOperator::Contains,
            FilterOperator::NotContains,
            FilterOperator::IContains,
        ];
        for operator in operators {
            let mut raw = query();
            let mut item = filter(operator);
            let GroupOrFilter::Filter(inner) = &mut item else {
                unreachable!()
            };
            inner.value = serde_json::json!({ "unexpected": "object" });
            raw.filters.values.push(item);

            let error = error(raw);
            assert_eq!(error.field, "filters.values[0].value");
            assert_eq!(
                error.message,
                "filter value must be a string for this operator"
            );
        }

        for operator in [FilterOperator::IsSet, FilterOperator::IsNotSet] {
            let mut raw = query();
            let mut item = filter(operator);
            let GroupOrFilter::Filter(inner) = &mut item else {
                unreachable!()
            };
            inner.value = serde_json::json!({ "ignored": true });
            raw.filters.values.push(item);
            SupportedQuery::try_from(raw).unwrap();
        }
    }

    #[test]
    fn accepts_at_most_one_bounded_event_breakdown() {
        let mut valid = query();
        valid.breakdown = Some(crate::query::ir::Breakdown {
            source: FilterSource::Event,
            key: "country".into(),
            limit: MAX_BREAKDOWN_LIMIT,
        });
        SupportedQuery::try_from(valid.clone()).unwrap();

        let mut second = valid.clone();
        second.breakdown2 = valid.breakdown.clone();
        assert_eq!(error(second).field, "breakdown2");

        let mut person = valid.clone();
        person.breakdown.as_mut().unwrap().source = FilterSource::Person;
        assert_eq!(error(person).field, "breakdown.source");

        let mut no_key = valid.clone();
        no_key.breakdown.as_mut().unwrap().key.clear();
        assert_eq!(error(no_key).field, "breakdown.key");

        let mut zero = valid.clone();
        zero.breakdown.as_mut().unwrap().limit = 0;
        assert_eq!(error(zero).field, "breakdown.limit");

        let mut too_large = valid;
        too_large.breakdown.as_mut().unwrap().limit = MAX_BREAKDOWN_LIMIT + 1;
        assert_eq!(error(too_large).field, "breakdown.limit");
    }

    #[test]
    fn requires_a_strict_absolute_rfc3339_range() {
        for interval in [
            Interval::Hour,
            Interval::Day,
            Interval::Week,
            Interval::Month,
        ] {
            let mut valid = query();
            valid.range.from = Some("2026-08-01T05:30:00+05:30".into());
            valid.interval = interval;
            SupportedQuery::try_from(valid).unwrap();
        }

        let mut relative = query();
        relative.range.last_n = Some(LastN::Days(7));
        assert_eq!(error(relative).field, "range.last_n");

        let mut missing_from = query();
        missing_from.range.from = None;
        assert_eq!(error(missing_from).field, "range.from");

        let mut missing_to = query();
        missing_to.range.to = None;
        assert_eq!(error(missing_to).field, "range.to");

        let mut invalid = query();
        invalid.range.from = Some("2026-08-01".into());
        assert_eq!(error(invalid).field, "range.from");

        let mut equal = query();
        equal.range.to = equal.range.from.clone();
        assert_eq!(error(equal).field, "range");

        let mut reversed = query();
        reversed.range.from = Some("2026-08-03T00:00:00Z".into());
        assert_eq!(error(reversed).field, "range");
    }

    #[test]
    fn normalizes_absolute_ranges_to_utc_before_hashing_or_execution() {
        let mut raw = query();
        raw.range.from = Some("2026-08-01T05:30:00+05:30".into());
        raw.range.to = Some("2026-08-02T05:30:00+05:30".into());

        let supported = SupportedQuery::try_from(raw).unwrap();

        assert_eq!(
            supported.as_query().range.from.as_deref(),
            Some("2026-08-01T00:00:00Z")
        );
        assert_eq!(
            supported.as_query().range.to.as_deref(),
            Some("2026-08-02T00:00:00Z")
        );
    }

    #[test]
    fn rejects_every_unimplemented_query_shape() {
        let mut formulas = query();
        formulas.formulas.push(crate::query::ir::Formula {
            label: "A".into(),
            expression: "A".into(),
        });

        let mut funnel = query();
        funnel.funnel_config = Some(crate::query::ir::FunnelConfig {
            order_type: crate::query::ir::FunnelOrder::Ordered,
            conversion_window_seconds: None,
            exclusions: vec![],
            attribution: crate::query::ir::FunnelAttribution::AllSteps,
        });

        let mut retention = query();
        retention.retention_config = Some(crate::query::ir::RetentionConfig::default());

        let mut lifecycle = query();
        lifecycle.lifecycle_config = Some(crate::query::ir::LifecycleConfig {
            event: crate::query::ir::EventMatch::Any,
            prior_period: crate::query::ir::LifecyclePeriod::Day,
        });

        let mut stickiness = query();
        stickiness.stickiness_config = Some(crate::query::ir::StickinessConfig {
            event: crate::query::ir::EventMatch::Any,
            window_days: 7,
        });

        let mut actors = query();
        actors.actors_config = Some(crate::query::ir::ActorsConfig {
            series_index: 0,
            day: String::new(),
            offset: 0,
            limit: 100,
        });

        let mut sql = query();
        sql.sql_config = Some(crate::query::ir::SqlConfig {
            sql: "SELECT 1".into(),
        });

        for (field, raw) in [
            ("formulas", formulas),
            ("funnel_config", funnel),
            ("retention_config", retention),
            ("lifecycle_config", lifecycle),
            ("stickiness_config", stickiness),
            ("actors_config", actors),
            ("sql_config", sql),
        ] {
            assert_eq!(error(raw).field, field);
        }
    }

    #[test]
    fn broad_ir_deserialization_precedes_supported_validation() {
        let json = serde_json::json!({
            "kind": "funnels",
            "series": [{
                "event": { "type": "name", "value": "signup" },
                "math": { "type": "total" }
            }],
            "range": {
                "from": "2026-08-01T00:00:00Z",
                "to": "2026-08-02T00:00:00Z"
            }
        });

        let raw: Query = serde_json::from_value(json).unwrap();
        let rejection = SupportedQuery::try_from(raw).unwrap_err();

        assert_eq!(
            serde_json::to_value(&rejection).unwrap(),
            serde_json::json!({
                "code": "unsupported_query",
                "field": "kind",
                "message": "only trends queries are supported"
            })
        );
        assert_eq!(
            rejection.to_string(),
            "unsupported_query at kind: only trends queries are supported"
        );
    }
}
