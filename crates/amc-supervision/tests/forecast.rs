use amc_supervision::forecast::{Forecast, Settings};

fn settings() -> Settings {
    Settings {
        horizon: 10,
        calibration: 32,
        alpha: 0.1,
        max_gap_ms: 1500,
    }
}

#[test]
fn plateau_and_reclaim_do_not_authorize_disruptive_recovery() {
    let mut model = Forecast::new(settings()).unwrap();
    for t in 0..300 {
        let value = 0.4 + ((t % 20) as f64) * 0.001;
        let report = model.observe(t * 1000, "one", &[value, 0.0]).unwrap();
        assert!(!report.strong, "plateau at {t}: {report:?}");
    }
    let report = model.observe(300_000, "one", &[0.1, 0.0]).unwrap();
    assert!(!report.strong);
}

#[test]
fn sustained_growth_is_predicted_before_the_boundary() {
    let mut model = Forecast::new(settings()).unwrap();
    let mut lead = None;
    for t in 0..150 {
        let value = 0.1 + t as f64 * 0.006;
        let report = model.observe(t * 1000, "one", &[value]).unwrap();
        if report.strong && value < 1.0 {
            lead.get_or_insert(150 - t);
        }
    }
    assert!(lead.is_some_and(|seconds| seconds >= 3), "lead={lead:?}");
}

#[test]
fn horizons_are_scored_only_after_their_future_arrives() {
    let mut model = Forecast::new(settings()).unwrap();
    for t in 0..10 {
        let report = model.observe(t * 1000, "one", &[0.2]).unwrap();
        assert_eq!(report.completed_windows, 0);
        assert!(!report.ready);
        assert!(!report.strong);
    }
    assert_eq!(
        model
            .observe(10_000, "one", &[0.2])
            .unwrap()
            .completed_windows,
        1
    );
}

#[test]
fn restart_and_sampling_gaps_censor_pending_windows() {
    let mut model = Forecast::new(settings()).unwrap();
    for t in 0..60 {
        model.observe(t * 1000, "one", &[0.2]).unwrap();
    }
    let report = model.observe(60_000, "two", &[0.02]).unwrap();
    assert!(!report.ready);
    assert!(report.censored_windows > 0);
    let report = model.observe(80_000, "two", &[0.02]).unwrap();
    assert!(!report.ready);
    assert_eq!(report.completed_windows, 50);
}

#[test]
fn missing_or_invalid_observations_are_not_zero_or_training_data() {
    let mut model = Forecast::new(settings()).unwrap();
    model.observe(1000, "one", &[0.2]).unwrap();
    assert!(model.observe(2000, "one", &[f64::NAN]).is_err());
    assert!(model.observe(2000, "one", &[]).is_err());
    assert!(model.observe(999, "one", &[0.2]).is_err());
}

#[test]
fn a_joint_window_score_includes_swap_and_other_boundaries() {
    let mut model = Forecast::new(settings()).unwrap();
    let mut detected = false;
    for t in 0..150 {
        let report = model
            .observe(t * 1000, "one", &[0.4, 0.1 + t as f64 * 0.006])
            .unwrap();
        detected |= report.strong && t < 150;
    }
    assert!(detected, "flat resident memory cannot hide growing swap");
}
