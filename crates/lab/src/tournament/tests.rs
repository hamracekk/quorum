use trader_core::Performance;

use super::*;

fn report(sharpe: f64, drawdown: f64, trades: usize, benchmark_sharpe: f64) -> Report {
    let perf = |sharpe: f64, max_drawdown: f64| Performance {
        total_return: 0.0,
        cagr: 0.0,
        max_drawdown,
        sharpe,
    };
    Report {
        initial_equity: 10_000.0,
        end_equity: 10_000.0,
        strategy: perf(sharpe, drawdown),
        benchmark: perf(benchmark_sharpe, 0.3),
        trades,
        fees: 0.0,
    }
}

fn standing(id: &str, days: f64, record: Report, champion: Report, losses: u32) -> Standing {
    Standing {
        id: id.into(),
        days,
        record,
        champion,
        losses,
    }
}

#[test]
fn the_best_eligible_challenger_is_promoted_and_only_one() {
    let champion = report(0.5, 0.10, 20, 0.2);
    let actions = review(
        &[
            standing(
                "good",
                40.0,
                report(1.0, 0.08, 15, 0.2),
                champion.clone(),
                0,
            ),
            standing(
                "better",
                35.0,
                report(1.4, 0.09, 12, 0.2),
                champion.clone(),
                0,
            ),
            standing(
                "riskier",
                60.0,
                report(2.0, 0.20, 30, 0.2),
                champion.clone(),
                0,
            ),
            standing("young", 10.0, report(3.0, 0.01, 30, 0.2), champion, 0),
        ],
        Rules::default(),
    );
    let promoted: Vec<&str> = actions
        .iter()
        .filter_map(|a| match a {
            Action::Promote { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(promoted, ["better"]);
    assert!(actions.iter().any(|a| matches!(
        a,
        Action::Keep { id, reason, .. } if id == "riskier" && reason.contains("drawdown")
    )));
    assert!(actions.iter().any(|a| matches!(
        a,
        Action::Keep { id, reason, .. } if id == "young" && reason.contains("days")
    )));
}

#[test]
fn challengers_trailing_buy_and_hold_twice_in_a_row_are_retired() {
    let champion = report(0.5, 0.10, 20, 0.8);
    let first = review(
        &[standing(
            "weak",
            30.0,
            report(0.2, 0.05, 15, 0.8),
            champion.clone(),
            0,
        )],
        Rules::default(),
    );
    assert_eq!(
        first,
        [Action::Keep {
            id: "weak".into(),
            losses: 1,
            reason: "Sharpe 0.20 not above champion 0.50".into()
        }]
    );
    let second = review(
        &[standing(
            "weak",
            37.0,
            report(0.2, 0.05, 15, 0.8),
            champion.clone(),
            1,
        )],
        Rules::default(),
    );
    assert!(matches!(&second[0], Action::Retire { id, .. } if id == "weak"));
    // Inside the grace period nothing counts against it.
    let fresh = review(
        &[standing(
            "new",
            3.0,
            report(-1.0, 0.05, 1, 0.8),
            champion,
            1,
        )],
        Rules::default(),
    );
    assert!(matches!(&fresh[0], Action::Keep { losses: 0, .. }));
}
