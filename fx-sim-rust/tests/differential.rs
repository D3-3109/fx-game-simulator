use fx_sim_rust::trading::{Account, Prices};
use serde_json::{Value, json};

fn compare(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            assert!((a - b).abs() <= 1e-8_f64.max(b.abs() * 1e-12), "{a} != {b}");
        }
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(a.len(), b.len());
            for (key, value) in b {
                compare(&a[key], value);
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len());
            for (a, b) in a.iter().zip(b) {
                compare(a, b);
            }
        }
        _ => assert_eq!(actual, expected),
    }
}

#[test]
fn fixed_quote_python_rust_accounting_differential() {
    let output = std::process::Command::new("python3")
        .args([
            "-B",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/python_reference.py"),
        ])
        .output()
        .expect("Python oracle required");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cases.len(), 18);
    for case in cases {
        let initial: Prices = serde_json::from_value(case["initial"].clone()).unwrap();
        let later: Prices = serde_json::from_value(case["later"].clone()).unwrap();
        let mut a = Account {
            balance: 10_000_000.0,
            ..Account::default()
        };
        let mut trades = vec![
            a.open(
                case["side"].as_str().unwrap(),
                0.1,
                case["symbol"].as_str().unwrap(),
                &initial,
                1,
                123,
            )
            .unwrap(),
        ];
        compare(
            &json!({"equity":a.equity(&initial).unwrap(),"used":a.used_margin(&initial).unwrap(),"margin_level":a.margin_level(&initial).unwrap(),"entry":a.positions[0].entry}),
            &case["opened"],
        );
        trades.extend(a.close(None, &later, 123).unwrap());
        compare(&json!(a.balance), &case["balance"]);
        compare(&json!(trades), &case["trades"]);
    }
}
