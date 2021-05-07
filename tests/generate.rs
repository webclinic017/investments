use std::fs::File;
use std::io::Write;

use retest::{Plan, DiffKind};

use investments::core::EmptyResult;

#[test]
fn generate_regression_tests() {
    let mut t = Tests::new();

    // deposits
    t.add("Deposits", "deposits");
    t.add("Deposits cron mode", "deposits --cron --date 01.01.2100");

    // show
    t.add("Show", "show ib");
    t.add("Show flat", "show ib --flat");

    // analyse
    t.add("Analyse", "analyse all --all");
    t.add("Analyse complex", "analyse ib-complex --all").config("other");

    // simulate-sell
    t.add("Simulate sell partial", "simulate-sell ib all VTI 50 BND 50 BND");
    t.add("Simulate sell in other currency", "simulate-sell tinkoff --base-currency USD");
    t.add("Simulate sell after stock split", "simulate-sell ib-stock-split all AAPL").config("other");
    t.add("Simulate sell after reverse stock split", "simulate-sell ib-reverse-stock-split all AAPL all VISL").config("other");
    t.add("Simulate sell zero cost position", "simulate-sell ib-complex 5 VTRS 125 VTRS").config("other");
    t.add("Simulate sell with mixed currency", "simulate-sell tinkoff-mixed-currency-trade all VTBA all VTBX").config("other");

    // tax-statement
    t.add("IB complex tax statement", "tax-statement ib-complex").config("other");
    t.add("IB stock split tax statement", "tax-statement ib-stock-split").config("other");
    t.add("IB reverse stock split tax statement", "tax-statement ib-reverse-stock-split").config("other");
    t.add("IB reverse stock split with reverse order tax statement", "tax-statement ib-reverse-stock-split-reverse-order").config("other");
    t.add("IB simple with LSE tax statement", "tax-statement ib-simple-with-lse").config("other");
    t.add("IB symbol with space tax statement", "tax-statement ib-symbol-with-space").config("other");
    t.add("IB tax remapping tax statement", "tax-statement ib-tax-remapping").config("other");
    t.add("IB trading tax statement", "tax-statement ib-trading").config("other");
    t.add("Tinkoff complex tax statement", "tax-statement tinkoff-complex").config("other");

    // cash-flow
    t.add("IB margin RUB cash flow", "cash-flow ib-margin-rub").config("other");
    t.add("IB stock split cash flow", "cash-flow ib-stock-split").config("other");
    t.add("IB reverse stock split cash flow", "cash-flow ib-reverse-stock-split").config("other");
    t.add("IB reverse stock split with reverse order cash flow", "cash-flow ib-reverse-stock-split-reverse-order").config("other");
    t.add("IB simple with LSE cash flow", "cash-flow ib-simple-with-lse").config("other");
    t.add("IB tax remapping cash flow", "cash-flow ib-tax-remapping").config("other");
    t.add("IB trading cash flow", "cash-flow ib-trading").config("other");
    t.add("Open inactive with forex trades cash flow", "cash-flow open-inactive-with-forex").config("other");
    t.add("Tinkoff complex cash flow", "cash-flow tinkoff-complex").config("other");

    // metrics
    t.add("Metrics", "metrics $OUT_PATH/metrics");

    let accounts = &[
        ("IB", Some(2018)),
        ("Firstrade", Some(2020)),

        ("IIA", None),
        ("BCS", None),
        ("Open", None),
        ("Tinkoff", None),

        ("Kate", None),
        ("Kate IIA", None),
    ];
    let end_tax_year = 2021;

    for &(name, start_tax_year) in accounts {
        let id = &name_to_id(name);

        t.with_args(&format!("Rebalance {}", name), &["rebalance", id]);
        t.with_args(&format!("Simulate sell {}", name), &["simulate-sell", id]);

        if let Some(start_tax_year) = start_tax_year {
            for tax_year in start_tax_year..=end_tax_year {
                let tax_year_string = &tax_year.to_string();

                t.with_args(
                    &format!("{} tax statement {}", name, tax_year),
                    &["tax-statement", id, tax_year_string],
                );
                t.tax_statement(name, tax_year);

                t.with_args(
                    &format!("{} cash flow {}", name, tax_year),
                    &["cash-flow", id, tax_year_string],
                );
            }
        } else {
            t.with_args(&format!("{} tax statement", name), &["tax-statement", id]);
            t.with_args(&format!("{} cash flow", name), &["cash-flow", id]);
        }
    }

    t.write().unwrap()
}

struct Tests {
    tests: Vec<Test>,
}

impl Tests {
    fn new() -> Tests {
        Tests { tests: Vec::new() }
    }

    fn add<'a>(&'a mut self, name: &str, args: &str) -> &'a mut Test {
        self.with_args(name, &args.split(' ').collect::<Vec<_>>())
    }

    fn with_args<'a>(&'a mut self, name: &str, args: &[&str]) -> &'a mut Test {
        self.tests.push(Test::new(name, "tests/investments", args));
        self.tests.last_mut().unwrap()
    }

    fn tax_statement(&mut self, name: &str, year: i32) -> &mut Test {
        let id = &name_to_id(name);
        let path = format!("$OUT_PATH/{}-tax-statement-{}.dc{}", id, year, year % 10);

        self.tests.push(Test::new(
            &format!("{} tax statement generation {}", name, year),
            "tests/test-tax-statement", &[id, &year.to_string(), &path],
        ));

        let test = self.tests.last_mut().unwrap();
        test.diff(DiffKind::Binary);
        test
    }

    fn write(self) -> EmptyResult {
        let mut plan = Plan::new()
            .expected_path("testdata/rt_expected")
            .actual_path("testdata/rt_actual")
            .build();

        for test in self.tests {
            plan.push(test.build());
        }

        let mut file = File::create("tests/rt.rt")?;
        file.write_all(plan.rt().as_bytes())?;
        file.flush()?;

        Ok(())
    }
}

struct Test {
    name: String,
    app: String,
    config: String,
    args: Vec<String>,
    diff: Option<DiffKind>,
}

impl Test {
    fn new(name: &str, app: &str, args: &[&str]) -> Test {
        Test {
            name: name.to_owned(),
            app: app.to_owned(),
            config: "main".to_owned(),
            args: args.iter().map(|&arg| arg.to_owned()).collect(),
            diff: None,
        }
    }

    fn config(&mut self, name: &str) -> &mut Test {
        self.config = name.to_owned();
        self
    }

    fn diff(&mut self, kind: DiffKind) -> &mut Test {
        self.diff.replace(kind);
        self
    }

    fn build(self) -> retest::Test {
        let mut stdout = true;
        for arg in &self.args {
            if arg.starts_with("$OUT_PATH/") {
                stdout = false;
                break;
            }
        }

        let mut args: Vec<&str> = vec![&self.config];
        args.extend(self.args.iter().map(|arg| arg.as_str()));

        let mut test = retest::Test::new(self.app)
            .name(&self.name)
            .args(&args)
            .build();

        if stdout {
            test.stdout(&name_to_id(&self.name));
        }

        if let Some(diff) = self.diff {
            test.diff(diff);
        }

        test
    }
}

fn name_to_id(name: &str) -> String {
    name.to_lowercase().replace(' ', "-")
}