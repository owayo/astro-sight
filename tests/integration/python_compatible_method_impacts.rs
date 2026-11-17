//! 所有型付きの互換なメソッド変更を hook の素の参照名へ対応付ける。

use super::support::*;

const BEFORE: &str =
    "class Order:\n    items = ()\n\n    def sealed(self):\n        return bool(self.items)\n";
const CONSUMER: &str = "from dataclasses import dataclass\nfrom shop.order import Order\n\n@dataclass\nclass Row:\n    order: Order\n    sealed: bool\n\ndef make_row(order):\n    return Row(order=order, sealed=order.sealed())\n\ndef usable(rows):\n    return [row for row in rows if row.sealed]\n\nclass Child(Order):\n    pass\n\ndef inherited(order: Child):\n    return order.sealed()\n\ndef unknown(value):\n    return value.sealed()\n";

fn method_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("shop");
    repo.write("shop/__init__.py", "");
    repo.write("shop/order.py", BEFORE);
    repo.write("shop/summary.py", CONSUMER);
    repo.init_git();
    repo.commit_all("initial");
    repo
}

fn changed_order(parameters: &str) -> String {
    format!(
        "class Order:\n    items = ()\n\n    def sealed(self{parameters}):\n        if limit is not None and len(self.items) > limit:\n            return False\n        return bool(self.items)\n"
    )
}

fn hook(repo: &TestRepo, extra: &[&str]) -> (Option<i32>, serde_json::Value) {
    let output = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .args(extra)
        .output()
        .unwrap();
    let json = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|error| panic!("invalid hook JSON: {error}: {output:?}"));
    (output.status.code(), json)
}

fn assert_compatible(repo: &TestRepo, extra: &[&str]) {
    let mut args = vec!["--git"];
    args.extend_from_slice(extra);
    let review = repo.run_json("review", &args);
    assert!(
        review["api_changes"]["compatible_modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["name"] == "Order.sealed"
                && change["reason"] == "trailing_optional_params"),
        "{review}"
    );
    let (code, json) = hook(repo, extra);
    assert_eq!(code, Some(0), "{extra:?}: {json}");
    assert!(json["impacts"].is_null(), "{json}");
    assert_eq!(json["blocking_categories"], serde_json::json!([]), "{json}");
    let refs = json["impact_info"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["src"] == "shop/order.py")
        .unwrap()["refs"]
        .as_array()
        .unwrap();
    for line in [6, 9, 12, 18, 21] {
        assert!(
            refs.iter()
                .any(|row| row["p"] == "shop/summary.py" && row["ln"] == line),
            "missing line {line}: {json}"
        );
    }
}

#[test]
fn python_compatible_method_impacts_preserve_raw_references_as_information() {
    for parameters in [", limit=None", ", *, limit=None"] {
        let repo = method_repo();
        let before = repo.run_json("refs", &["--name", "sealed"]);
        repo.write("shop/order.py", changed_order(parameters));
        assert_compatible(&repo, &[]);
        let after = repo.run_json("refs", &["--name", "sealed"]);
        let summary_refs = |result: &serde_json::Value| {
            result["refs"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["path"] == "shop/summary.py")
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(summary_refs(&before).len(), 6, "{before}");
        assert_eq!(summary_refs(&before), summary_refs(&after), "{after}");
    }
}

#[test]
fn python_compatible_method_impacts_use_custom_base_and_index_sources() {
    for staged in [false, true] {
        let repo = method_repo();
        repo.write("shop/order.py", changed_order(", limit=None"));
        repo.commit_all("optional parameter");
        if staged {
            repo.write("shop/order.py", changed_order(", *, limit=None"));
            repo.stage_all();
            repo.write("shop/order.py", changed_order(", limit"));
            assert_compatible(&repo, &["--base", "HEAD~1", "--staged"]);
            repo.stage_all();
            repo.write("shop/order.py", changed_order(", *, limit=None"));
            let (code, json) = hook(&repo, &["--base", "HEAD~1", "--staged"]);
            assert_eq!(code, Some(1), "{json}");
            assert!(json["api"]["mod_compat"].is_null(), "{json}");
            assert!(json["impacts"].is_array(), "{json}");
        } else {
            assert_compatible(&repo, &["--base", "HEAD~1"]);
        }
    }
}

#[test]
fn python_compatible_method_impacts_keep_unproven_same_name_changes_blocking() {
    for (other_before, other_after) in [
        (
            "class Other:\n    def sealed(self):\n        return True\n",
            "class Other:\n    def sealed(self, required):\n        return True\n",
        ),
        (
            "class _Private:\n    def sealed(self):\n        return True\n",
            "class _Private:\n    def sealed(self, required):\n        return True\n",
        ),
        (
            "def factory():\n    def sealed():\n        return True\n    return sealed\n",
            "def factory():\n    def sealed(required):\n        return True\n    return sealed\n",
        ),
    ] {
        for same_file in [false, true] {
            let repo = method_repo();
            let path = if same_file {
                "shop/order.py"
            } else {
                "shop/other.py"
            };
            let before = if same_file {
                format!("{BEFORE}\n{other_before}")
            } else {
                other_before.to_owned()
            };
            repo.write(path, before);
            repo.commit_all("other owner");
            let after = changed_order(", *, limit=None");
            repo.write("shop/order.py", &after);
            repo.write(
                path,
                if same_file {
                    format!("{after}\n{other_after}")
                } else {
                    other_after.to_owned()
                },
            );
            let (code, json) = hook(&repo, &[]);
            if same_file || !other_before.starts_with("def factory") {
                assert_eq!(code, Some(1), "{same_file}/{other_after}: {json}");
            }
            assert!(
                json["impact_info"].as_array().is_none_or(|groups| groups
                    .iter()
                    .all(|group| group["src"] != "shop/order.py")),
                "{json}"
            );
        }
    }
}

#[test]
fn python_compatible_method_impacts_keep_private_same_signature_body_changes_blocking() {
    for same_file in [false, true] {
        let repo = method_repo();
        let before = "class _Private:\n    def sealed(self):\n        return True\n";
        let after = before.replace("True", "False");
        let path = if same_file {
            "shop/order.py"
        } else {
            "shop/other.py"
        };
        repo.write(
            path,
            if same_file {
                format!("{BEFORE}\n{before}")
            } else {
                before.to_owned()
            },
        );
        repo.write(
            "consumer.py",
            "def use(value):\n    return value.sealed()\n",
        );
        repo.commit_all("private owner");
        let order = changed_order(", *, limit=None");
        repo.write("shop/order.py", &order);
        repo.write(
            path,
            if same_file {
                format!("{order}\n{after}")
            } else {
                after
            },
        );
        let (code, json) = hook(&repo, &[]);
        if same_file {
            assert_eq!(code, Some(1), "{same_file}: {json}");
        }
        assert!(
            json["impact_info"]
                .as_array()
                .is_none_or(|groups| groups.iter().all(|group| group["src"] != path)),
            "{json}"
        );
    }
}
