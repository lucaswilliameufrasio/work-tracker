use std::collections::BTreeSet;

use puxiri::http::openapi::browser_document;
use serde_json::Value;

fn check_references(root: &Value, value: &Value) {
    match value {
        Value::Object(fields) => {
            if let Some(reference) = fields.get("$ref") {
                let reference = reference.as_str().unwrap();
                assert!(reference.starts_with("#/components/"));
                assert!(
                    root.pointer(&reference[1..]).is_some(),
                    "unresolved: {reference}"
                );
            }
            for child in fields.values() {
                check_references(root, child);
            }
        }
        Value::Array(items) => {
            for child in items {
                check_references(root, child);
            }
        }
        _ => {}
    }
}

#[test]
fn generated_contract_has_unique_operations_resolved_refs_and_only_implemented_routes() {
    let document = browser_document();
    assert_eq!(document["openapi"], "3.1.0");
    assert_eq!(
        document["components"]["securitySchemes"]["browserSession"]["in"],
        "cookie"
    );
    assert_eq!(
        document["components"]["securitySchemes"]["browserSession"]["name"],
        "__Host-puxiri_session"
    );
    check_references(&document, &document);
    let expected = BTreeSet::from([
        ("/api/v1/auth/session", "get"),
        ("/api/v1/auth/session", "head"),
        ("/api/v1/auth/session", "delete"),
        ("/api/v1/auth/sessions", "get"),
        ("/api/v1/auth/sessions", "head"),
        ("/api/v1/auth/sessions", "delete"),
        ("/api/v1/auth/sessions/{session_id}", "delete"),
        ("/api/v1/projects", "get"),
        ("/api/v1/projects", "head"),
        ("/api/v1/projects/{project_id}/capabilities", "get"),
        ("/api/v1/projects/{project_id}/capabilities", "head"),
        ("/api/v1/projects/{project_id}/board", "get"),
        ("/api/v1/projects/{project_id}/board", "head"),
        ("/api/v1/projects/{project_id}/board/{status_id}", "get"),
        ("/api/v1/projects/{project_id}/board/{status_id}", "head"),
        ("/api/v1/projects/{project_id}/issues", "get"),
        ("/api/v1/projects/{project_id}/issues", "head"),
        ("/api/v1/projects/{project_id}/issues", "post"),
        (
            "/api/v1/projects/{project_id}/issues/{issue_number}/text",
            "put",
        ),
        (
            "/api/v1/projects/{project_id}/issues/{issue_number}/priority",
            "put",
        ),
        (
            "/api/v1/projects/{project_id}/issues/{issue_number}/due-date",
            "put",
        ),
        (
            "/api/v1/projects/{project_id}/issues/{issue_number}/move",
            "post",
        ),
        ("/api/v1/projects/{project_id}/statuses", "get"),
        ("/api/v1/projects/{project_id}/statuses", "head"),
        ("/api/v1/projects/{project_id}/issue-types", "get"),
        ("/api/v1/projects/{project_id}/issue-types", "head"),
        ("/api/v1/projects/{project_id}/statuses/{record_id}", "get"),
        ("/api/v1/projects/{project_id}/statuses/{record_id}", "head"),
        (
            "/api/v1/projects/{project_id}/issue-types/{record_id}",
            "get",
        ),
        (
            "/api/v1/projects/{project_id}/issue-types/{record_id}",
            "head",
        ),
        ("/api/v1/projects/{project_id}/issues/{issue_number}", "get"),
        (
            "/api/v1/projects/{project_id}/issues/{issue_number}",
            "head",
        ),
        (
            "/api/v1/projects/{project_id}/issues/{issue_number}/history",
            "get",
        ),
        (
            "/api/v1/projects/{project_id}/issues/{issue_number}/history",
            "head",
        ),
        ("/api/v1/admin/projects", "get"),
        ("/api/v1/admin/projects", "head"),
    ]);
    let mut actual = BTreeSet::new();
    let mut operation_ids = BTreeSet::new();
    for (path, operations) in document["paths"].as_object().unwrap() {
        for (method, operation) in operations.as_object().unwrap() {
            actual.insert((path.as_str(), method.as_str()));
            assert!(operation_ids.insert(operation["operationId"].as_str().unwrap()));
            assert_eq!(
                operation.get("requestBody").is_some(),
                method == "post" || method == "put"
            );
            if method == "post" {
                let is_create = operation["operationId"] == "createProjectIssue";
                assert_eq!(operation["responses"].get("201").is_some(), is_create);
                assert_eq!(operation["responses"].get("200").is_some(), !is_create);
                for status in ["409", "415", "422", "503"] {
                    assert!(operation["responses"].get(status).is_some());
                }
            }
            assert!(
                operation["responses"].get("429").is_none(),
                "rate budgets are not implemented"
            );
            if method == "delete" {
                assert!(operation["responses"].get("200").is_none());
                assert!(operation["responses"]["204"].get("content").is_none());
                let parameters = operation["parameters"].as_array().unwrap();
                for name in ["Origin", "x-csrf-token"] {
                    assert!(parameters.iter().any(
                        |parameter| parameter["name"] == name && parameter["required"] == true
                    ));
                }
            }
            if method == "head" {
                for response in operation["responses"].as_object().unwrap().values() {
                    assert!(response.get("content").is_none());
                }
            }
        }
    }
    assert_eq!(actual, expected);
    let due_date =
        &document["paths"]["/api/v1/projects/{project_id}/issues/{issue_number}/due-date"]["put"];
    assert_eq!(due_date["operationId"], "setProjectIssueDueDate");
    assert_eq!(
        due_date["requestBody"]["content"]["application/json"]["schema"]["required"],
        serde_json::json!(["expected_version", "due_date"])
    );
    assert_eq!(
        document["components"]["schemas"]["IssueDueDateReceipt"]["required"],
        serde_json::json!([
            "id",
            "project_id",
            "issue_number",
            "issue_key",
            "due_date",
            "result_version",
            "changed"
        ])
    );
    assert!(
        document["components"]["schemas"]["Issue"]["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("is_overdue"))
    );
    let round_trip: Value =
        serde_json::from_str(&serde_json::to_string_pretty(&document).unwrap()).unwrap();
    assert_eq!(round_trip, document);
}
