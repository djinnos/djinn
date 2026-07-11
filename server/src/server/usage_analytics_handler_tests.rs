use super::*;

fn empty_query() -> UsageQuery {
    UsageQuery {
        preset: None,
        start: None,
        end: None,
        granularity: None,
        project_id: None,
        model: None,
        agent_type: None,
        user_id: None,
    }
}

#[test]
fn granularity_parses_known_values() {
    assert_eq!(Granularity::parse(None).unwrap(), Granularity::Day);
    assert_eq!(Granularity::parse(Some("DAY")).unwrap(), Granularity::Day);
    assert_eq!(Granularity::parse(Some("week")).unwrap(), Granularity::Week);
    assert_eq!(
        Granularity::parse(Some("month")).unwrap(),
        Granularity::Month
    );
}

#[test]
fn granularity_rejects_unknown() {
    let err = Granularity::parse(Some("hour")).unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert!(err.1.contains("hour"));
}

#[test]
fn into_typed_maps_model_filter_and_drops_blanks() {
    let q = UsageQuery {
        start: Some("2025-01-01".into()),
        end: Some("2025-02-01".into()),
        granularity: Some("day".into()),
        project_id: Some("proj-1".into()),
        model: Some("".into()),
        ..empty_query()
    };
    let (typed, gran) = q.into_typed().unwrap();
    assert_eq!(typed.from, "2025-01-01");
    assert_eq!(typed.to, "2025-02-01");
    assert_eq!(typed.project_id.as_deref(), Some("proj-1"));
    assert!(typed.model_id.is_none(), "blank model must be dropped");
    assert_eq!(gran, Granularity::Day);
}

#[test]
fn into_typed_supplies_default_window_when_omitted() {
    let (typed, gran) = empty_query().into_typed().unwrap();
    assert!(!typed.from.is_empty());
    assert!(!typed.to.is_empty());
    assert_eq!(gran, Granularity::Day);
}

#[test]
fn preset_30d_window_is_wider_than_7d() {
    let span = |preset: &str| {
        let q = UsageQuery {
            preset: Some(preset.into()),
            ..empty_query()
        };
        let (typed, _) = q.into_typed().unwrap();
        let from = parse_iso_date_prefix("from", &typed.from).unwrap();
        let to = parse_iso_date_prefix("to", &typed.to).unwrap();
        to.to_julian_day() - from.to_julian_day()
    };
    assert!(span("30d") > span("7d"));
}

#[test]
fn invalid_preset_returns_400() {
    let q = UsageQuery {
        preset: Some("90d".into()),
        ..empty_query()
    };
    let err = q.into_typed().unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert!(err.1.contains("90d"));
}

#[test]
fn into_typed_rejects_reversed_custom_range() {
    let q = UsageQuery {
        start: Some("2025-03-01".into()),
        end: Some("2025-03-01".into()),
        ..empty_query()
    };
    let err = q.into_typed().unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert!(err.1.contains("from must be before to"));
}

#[test]
fn into_typed_rejects_invalid_date() {
    let q = UsageQuery {
        start: Some("2025-02-30".into()),
        end: Some("2025-03-01".into()),
        ..empty_query()
    };
    let err = q.into_typed().unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
}

#[test]
fn previous_window_matches_requested_day_span() {
    let query = UsageAnalyticsQuery {
        from: "2025-03-10".into(),
        to: "2025-03-17".into(),
        group_by: GroupDimension::Model,
        project_id: Some("proj-1".into()),
        model_id: Some("model-1".into()),
        agent_type: Some("worker".into()),
        user_id: Some("user-1".into()),
    };
    let previous = previous_window_query(&query).unwrap();
    assert_eq!(previous.from, "2025-03-03");
    assert_eq!(previous.to, "2025-03-10");
    assert_eq!(previous.project_id.as_deref(), Some("proj-1"));
    assert_eq!(previous.user_id.as_deref(), Some("user-1"));
}

#[test]
fn build_kpis_emits_six_cards_with_deltas() {
    let totals = UsageTotals {
        session_count: 10,
        tokens_in: 100,
        tokens_out: 100,
        cache_read_tokens: 50,
        cache_write_tokens: 5,
        actual_spend_usd: Some(12.0),
        projected_usd: Some(8.0),
        list_price_usd: Some(20.0),
        unpriced_session_count: 0,
    };
    let previous = UsageTotals {
        session_count: 5,
        tokens_in: 50,
        tokens_out: 50,
        cache_read_tokens: 25,
        cache_write_tokens: 0,
        actual_spend_usd: Some(6.0),
        projected_usd: Some(4.0),
        list_price_usd: Some(10.0),
        unpriced_session_count: 0,
    };
    let kpis = build_kpis(&totals, &previous);
    assert_eq!(kpis.len(), 6);

    // The primary card leads with the combined list-price figure.
    let list_price_kpi = &kpis[0];
    assert_eq!(list_price_kpi.label, "Total cost (list-price)");
    assert_eq!(list_price_kpi.value, Some(20.0));
    assert_eq!(list_price_kpi.list_price_usd, Some(20.0));
    assert!(
        (list_price_kpi.delta_pct.unwrap() - 1.0).abs() < 1e-9,
        "doubled list-price cost = +100%"
    );
    assert!(list_price_kpi.formatted.starts_with('$'));
    // Primary caption surfaces the real API spend subset.
    assert!(
        list_price_kpi
            .caption
            .as_deref()
            .unwrap()
            .contains("real API spend")
    );

    let actual_kpi = &kpis[1];
    assert_eq!(actual_kpi.label, "Actual API Spend");
    assert_eq!(actual_kpi.value, Some(12.0));
    assert!(
        (actual_kpi.delta_pct.unwrap() - 1.0).abs() < 1e-9,
        "doubled actual spend = +100%"
    );
    assert!(actual_kpi.formatted.starts_with('$'));

    let projected_kpi = &kpis[2];
    assert_eq!(projected_kpi.label, "Projected Cost");
    assert_eq!(projected_kpi.value, Some(8.0));
    assert!(
        (projected_kpi.delta_pct.unwrap() - 1.0).abs() < 1e-9,
        "doubled projected = +100%"
    );

    let tokens = &kpis[3];
    assert_eq!(tokens.value, Some(200.0));
    assert!(tokens.formatted.is_empty());
}

#[test]
fn build_kpis_null_spend_yields_null_value_and_delta() {
    let totals = UsageTotals::default();
    let kpis = build_kpis(&totals, &UsageTotals::default());
    assert_eq!(kpis[0].label, "Total cost (list-price)");
    assert!(kpis[0].value.is_none());
    assert!(kpis[0].delta_pct.is_none());
    assert!(kpis[0].formatted.is_empty());
    assert_eq!(kpis[1].label, "Actual API Spend");
    assert!(kpis[1].value.is_none());
    assert!(kpis[1].delta_pct.is_none());
    assert_eq!(kpis[2].label, "Projected Cost");
    assert!(kpis[2].value.is_none());
    // Previous window empty → token delta is null, not a divide-by-zero.
    assert!(kpis[3].delta_pct.is_none());
}

#[test]
fn combine_list_price_is_null_safe() {
    // Both absent → None (mirrors SQL SUM FILTER over no rows).
    assert_eq!(combine_list_price(None, None), None);
    // One side present → the other counts as 0.
    assert_eq!(combine_list_price(Some(5.0), None), Some(5.0));
    assert_eq!(combine_list_price(None, Some(3.0)), Some(3.0));
    // Both present → sum.
    assert_eq!(combine_list_price(Some(5.0), Some(3.0)), Some(8.0));
}

#[test]
fn cost_caption_notes_excluded_unpriced_sessions() {
    assert_eq!(cost_caption(0, "Test"), "Test");
    assert_eq!(
        cost_caption(1, "Real API-key spend at list rates"),
        "Real API-key spend at list rates · 1 unpriced session excluded"
    );
    assert_eq!(
        cost_caption(5, "Real API-key spend at list rates"),
        "Real API-key spend at list rates · 5 unpriced sessions excluded"
    );
}

#[test]
fn build_kpis_cost_caption_reports_unpriced_count() {
    let totals = UsageTotals {
        actual_spend_usd: Some(12.5),
        unpriced_session_count: 3,
        ..UsageTotals::default()
    };
    let kpis = build_kpis(&totals, &UsageTotals::default());
    // The Actual API Spend card (index 1 after the leading list-price card)
    // reports the excluded-unpriced-session count.
    assert!(
        kpis[1]
            .caption
            .as_deref()
            .unwrap()
            .contains("3 unpriced sessions excluded")
    );
}

fn series_row(
    day: &str,
    model: &str,
    actual: Option<f64>,
    projected: Option<f64>,
    unpriced: i64,
) -> SeriesDetailRow {
    SeriesDetailRow {
        day: day.into(),
        model: model.into(),
        project_id: "p1".into(),
        project_name: "Proj One".into(),
        agent_type: "worker".into(),
        session_count: 1,
        tokens_in: 10,
        tokens_out: 5,
        cache_read_tokens: 3,
        task_count: 1,
        actual_spend_usd: actual,
        projected_usd: projected,
        list_price_usd: combine_list_price(actual, projected),
        unpriced_session_count: unpriced,
    }
}

#[test]
fn rollup_series_day_is_identity_per_dimension() {
    let rows = vec![
        series_row("2025-03-10", "a", Some(1.0), None, 0),
        series_row("2025-03-10", "b", None, Some(2.0), 0),
    ];
    let points = rollup_series(rows, Granularity::Day).unwrap();
    assert_eq!(points.len(), 2);
    assert!(points.iter().all(|p| p.date == "2025-03-10"));
}

#[test]
fn rollup_series_week_groups_same_dimension_and_splits_costs() {
    let rows = vec![
        series_row("2025-03-03", "a", Some(1.0), None, 0),
        series_row("2025-03-05", "a", None, None, 1), // unpriced
        series_row("2025-03-04", "b", None, Some(3.0), 0),
    ];
    let points = rollup_series(rows, Granularity::Week).unwrap();
    let a = points.iter().find(|p| p.model == "a").unwrap();
    assert_eq!(a.date, "2025-03-03");
    assert_eq!(a.tokens_in, 20);
    assert_eq!(a.actual_spend_usd, Some(1.0));
    assert!(a.projected_usd.is_none());
    assert_eq!(a.unpriced_session_count, 1);
    let b = points.iter().find(|p| p.model == "b").unwrap();
    assert_eq!(b.actual_spend_usd, None);
    assert_eq!(b.projected_usd, Some(3.0));
    assert_eq!(b.unpriced_session_count, 0);
}

#[test]
fn breakdown_row_sets_links_and_cost_per_task() {
    let row = EntityBreakdownRow {
        id: "task-1".into(),
        name: "A task".into(),
        actual_spend_usd: Some(3.0),
        projected_usd: Some(1.0),
        list_price_usd: Some(4.0),
        unpriced_session_count: 0,
        tokens_in: 10,
        tokens_out: 10,
        cache_read_tokens: 4,
        task_count: 2,
        success_rate: Some(0.5),
        avg_reopens: Some(1.0),
    };
    let task = breakdown_row(row.clone(), GroupDimension::Task);
    assert_eq!(task.task_id.as_deref(), Some("task-1"));
    assert!(task.proposal_id.is_none());
    assert_eq!(task.actual_cost_per_task, Some(1.5)); // 3.0 / 2
    assert_eq!(task.list_price_cost_per_task, Some(2.0)); // 4.0 / 2
    assert_eq!(task.list_price_usd, Some(4.0));

    let proposal = breakdown_row(row.clone(), GroupDimension::Proposal);
    assert_eq!(proposal.proposal_id.as_deref(), Some("task-1"));
    assert!(proposal.task_id.is_none());

    let user = breakdown_row(row, GroupDimension::User);
    assert!(user.task_id.is_none() && user.proposal_id.is_none());
}

#[test]
fn breakdown_row_zero_tasks_has_null_cost_per_task() {
    let row = EntityBreakdownRow {
        id: "u1".into(),
        name: "user".into(),
        actual_spend_usd: Some(3.0),
        projected_usd: None,
        list_price_usd: Some(3.0),
        unpriced_session_count: 0,
        tokens_in: 1,
        tokens_out: 1,
        cache_read_tokens: 0,
        task_count: 0,
        success_rate: None,
        avg_reopens: None,
    };
    let dto = breakdown_row(row, GroupDimension::User);
    assert!(dto.actual_cost_per_task.is_none());
    assert!(dto.list_price_cost_per_task.is_none());
}

#[test]
fn model_effectiveness_dto_renames_to_frontend_contract() {
    let row = ModelEffectivenessRow {
        model_id: "gpt".into(),
        sessions: 4,
        actual_spend_usd: Some(1.5),
        projected_usd: Some(0.5),
        list_price_usd: Some(2.0),
        unpriced_session_count: 0,
        tokens_in: 100,
        tokens_out: 50,
        cache_read_tokens: 40,
        shared_credit_completed_task_count: 3,
        success_rate: Some(0.75),
        avg_reopens: Some(0.2),
        first_pass_rejection_rate: Some(0.25),
        final_pass_share: Some(0.5),
        first_pass_rejected_session_count: 1,
        final_pass_completed_task_count: 1,
        actual_cost_per_completed_task: Some(0.5),
        list_price_cost_per_completed_task: Some(2.0 / 3.0),
        tokens_per_task: Some(50.0),
    };
    let dto: ModelEffectivenessDto = row.into();
    let json = serde_json::to_value(&dto).unwrap();
    assert_eq!(json.get("model").unwrap().as_str().unwrap(), "gpt");
    assert_eq!(json.get("task_count").unwrap().as_i64().unwrap(), 3);
    assert_eq!(
        json.get("completed_task_count").unwrap().as_i64().unwrap(),
        3
    );
    assert_eq!(json.get("session_count").unwrap().as_i64().unwrap(), 4);
    assert_eq!(json.get("total_tokens").unwrap().as_i64().unwrap(), 150);
    assert!((json.get("actual_spend_usd").unwrap().as_f64().unwrap() - 1.5).abs() < 1e-9);
    assert!((json.get("projected_usd").unwrap().as_f64().unwrap() - 0.5).abs() < 1e-9);
    assert!((json.get("list_price_usd").unwrap().as_f64().unwrap() - 2.0).abs() < 1e-9);
    assert!((json.get("actual_cost_per_task").unwrap().as_f64().unwrap() - 0.5).abs() < 1e-9);
    assert!(
        (json
            .get("list_price_cost_per_task")
            .unwrap()
            .as_f64()
            .unwrap()
            - 2.0 / 3.0)
            .abs()
            < 1e-9
    );
    // Repository-only names must not leak.
    assert!(json.get("model_id").is_none());
    assert!(json.get("spend_usd").is_none());
}

#[test]
fn project_model_cell_dto_derives_cost_per_task_and_renames() {
    let row = ProjectModelMatrixRow {
        project_id: "p1".into(),
        project_name: "Proj".into(),
        model_id: "m1".into(),
        sessions: 2,
        actual_spend_usd: Some(4.0),
        projected_usd: Some(2.0),
        list_price_usd: Some(6.0),
        unpriced_session_count: 0,
        tokens_in: 30,
        tokens_out: 20,
        cache_read_tokens: 12,
        task_count: 3,
        success_rate: Some(1.0),
        avg_reopens: Some(0.0),
    };
    let dto: ProjectModelCellDto = row.into();
    let json = serde_json::to_value(&dto).unwrap();
    assert_eq!(json.get("model").unwrap().as_str().unwrap(), "m1");
    assert_eq!(json.get("project_name").unwrap().as_str().unwrap(), "Proj");
    assert_eq!(json.get("total_tokens").unwrap().as_i64().unwrap(), 50);
    assert!(
        (json.get("actual_cost_per_task").unwrap().as_f64().unwrap() - (4.0 / 3.0)).abs() < 1e-9
    );
    assert!(
        (json
            .get("list_price_cost_per_task")
            .unwrap()
            .as_f64()
            .unwrap()
            - (6.0 / 3.0))
            .abs()
            < 1e-9
    );
    assert!((json.get("actual_spend_usd").unwrap().as_f64().unwrap() - 4.0).abs() < 1e-9);
    assert!((json.get("projected_usd").unwrap().as_f64().unwrap() - 2.0).abs() < 1e-9);
    assert!((json.get("list_price_usd").unwrap().as_f64().unwrap() - 6.0).abs() < 1e-9);
}

#[test]
fn response_serialises_with_frontend_contract_fields() {
    let response = UsageResponse {
        kpis: build_kpis(&UsageTotals::default(), &UsageTotals::default()),
        time_series: Vec::new(),
        breakdowns: BreakdownsDto {
            by_user: Vec::new(),
            by_project: Vec::new(),
            by_proposal: Vec::new(),
            by_task: Vec::new(),
        },
        model_effectiveness: Vec::new(),
        project_model_matrix: Vec::new(),
        generated_at: "2025-06-20T00:00:00Z".into(),
        unpriced_session_count: 0,
    };
    let json = serde_json::to_value(&response).unwrap();
    for field in [
        "kpis",
        "time_series",
        "breakdowns",
        "model_effectiveness",
        "project_model_matrix",
        "generated_at",
        "unpriced_session_count",
    ] {
        assert!(json.get(field).is_some(), "missing field {field}");
    }
    let breakdowns = json.get("breakdowns").unwrap();
    for field in ["by_user", "by_project", "by_proposal", "by_task"] {
        assert!(breakdowns.get(field).unwrap().is_array(), "missing {field}");
    }
}

#[test]
fn build_kpis_split_aggregates_on_exactly_one_contributor_each() {
    let totals = UsageTotals {
        session_count: 10,
        tokens_in: 100,
        tokens_out: 50,
        cache_read_tokens: 20,
        cache_write_tokens: 5,
        actual_spend_usd: Some(12.50),
        projected_usd: Some(8.75),
        list_price_usd: Some(21.25),
        unpriced_session_count: 3,
    };
    let kpis = build_kpis(&totals, &UsageTotals::default());

    // Total cost (list-price) card carries list_price_usd only.
    let list_price_kpi = &kpis[0];
    assert_eq!(list_price_kpi.label, "Total cost (list-price)");
    assert_eq!(list_price_kpi.list_price_usd, Some(21.25));
    assert!(list_price_kpi.actual_spend_usd.is_none());
    assert!(list_price_kpi.projected_usd.is_none());
    assert!(list_price_kpi.unpriced_count.is_none());

    // Actual API Spend card carries actual_spend_usd only.
    let actual_kpi = &kpis[1];
    assert_eq!(actual_kpi.actual_spend_usd, Some(12.50));
    assert!(actual_kpi.projected_usd.is_none());
    assert!(actual_kpi.list_price_usd.is_none());
    assert!(actual_kpi.unpriced_count.is_none());

    // Projected Cost card carries projected_usd only.
    let projected_kpi = &kpis[2];
    assert!(projected_kpi.actual_spend_usd.is_none());
    assert_eq!(projected_kpi.projected_usd, Some(8.75));
    assert!(projected_kpi.list_price_usd.is_none());
    assert!(projected_kpi.unpriced_count.is_none());

    // Tokens card has no split aggregates.
    let tokens_kpi = &kpis[3];
    assert!(tokens_kpi.actual_spend_usd.is_none());
    assert!(tokens_kpi.projected_usd.is_none());
    assert!(tokens_kpi.unpriced_count.is_none());

    // Sessions card carries unpriced_count only.
    let sessions_kpi = &kpis[4];
    assert_eq!(sessions_kpi.label, "Sessions");
    assert!(sessions_kpi.actual_spend_usd.is_none());
    assert!(sessions_kpi.projected_usd.is_none());
    assert_eq!(sessions_kpi.unpriced_count, Some(3));

    // Cache reads card has no split aggregates.
    let cache_kpi = &kpis[5];
    assert_eq!(cache_kpi.label, "Cache reads");
    assert!(cache_kpi.actual_spend_usd.is_none());
    assert!(cache_kpi.projected_usd.is_none());
    assert!(cache_kpi.unpriced_count.is_none());
}

#[test]
fn split_aggregates_omit_when_none_and_unpriced_present_at_zero() {
    let totals = UsageTotals::default(); // all zero/None
    let kpis = build_kpis(&totals, &UsageTotals::default());

    // List-price / Actual / Projected KPIs have None for their split fields
    // — serde omits them.
    assert!(kpis[0].list_price_usd.is_none());
    assert!(kpis[1].actual_spend_usd.is_none());
    assert!(kpis[2].projected_usd.is_none());

    // Sessions KPI carries unpriced_count even when 0 (top-level contract).
    assert_eq!(kpis[4].unpriced_count, Some(0));

    // Serialize and verify omitted fields don't appear as null.
    let json = serde_json::to_value(kpis[0].label.clone()).unwrap();
    assert!(json.is_string()); // sanity — the KPI serialization is tested below.

    // Serialize the full KPI array and verify structure.
    let json_arr = serde_json::to_value(&kpis).unwrap();
    let arr = json_arr.as_array().unwrap();
    // list_price_usd should be absent from the leading card (None → skip).
    assert!(arr[0].get("list_price_usd").is_none());
    // actual_spend_usd should be absent from the "Actual API Spend" card.
    assert!(arr[1].get("actual_spend_usd").is_none());
    // projected_usd should be absent from "Projected Cost" card.
    assert!(arr[2].get("projected_usd").is_none());
    // unpriced_count should be present on Sessions card as 0.
    assert_eq!(arr[4].get("unpriced_count").unwrap().as_i64().unwrap(), 0);
    // unpriced_count absent from non-Sessions KPIs.
    assert!(arr[0].get("unpriced_count").is_none());
    assert!(arr[3].get("unpriced_count").is_none());
    assert!(arr[5].get("unpriced_count").is_none());
}

#[test]
fn usage_response_serialises_top_level_unpriced_session_count() {
    let totals = UsageTotals {
        unpriced_session_count: 7,
        ..UsageTotals::default()
    };
    let response = UsageResponse {
        kpis: build_kpis(&totals, &UsageTotals::default()),
        time_series: Vec::new(),
        breakdowns: BreakdownsDto {
            by_user: Vec::new(),
            by_project: Vec::new(),
            by_proposal: Vec::new(),
            by_task: Vec::new(),
        },
        model_effectiveness: Vec::new(),
        project_model_matrix: Vec::new(),
        generated_at: "2025-06-20T00:00:00Z".into(),
        unpriced_session_count: totals.unpriced_session_count,
    };
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(
        json.get("unpriced_session_count")
            .unwrap()
            .as_i64()
            .unwrap(),
        7
    );
}
