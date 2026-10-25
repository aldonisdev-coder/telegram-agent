use serde_json::Value;
use std::time::Duration;

pub struct CryptoPay {
    token: String,
    client: reqwest::Client,
    base_url: String,
}

impl CryptoPay {
    pub fn new(token: String) -> Self {
        Self::with_base_url(token, "https://pay.crypt.bot/api")
    }

    /// Like [`CryptoPay::new`], but targets a custom API base; used by tests
    /// to point the client at a local mock server. `new()` stays canonical.
    pub fn with_base_url(token: String, base: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        Self {
            token,
            client,
            base_url: base.to_string(),
        }
    }

    async fn call(&self, method: &str, body: Option<Value>) -> Result<Value, String> {
        let url = format!("{}/{}", self.base_url, method);
        let mut req = self
            .client
            .post(&url)
            .header("Crypto-Pay-API-Token", &self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {}", e, text))?;
        if !status.is_success() {
            return Err(format!("HTTP {} - {}", status, text));
        }
        if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
            let err = v
                .get("error")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown error");
            return Err(err.to_string());
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn get_me(&self) -> Result<Value, String> {
        self.call("getMe", None).await
    }

    pub async fn get_balance(&self) -> Result<Value, String> {
        self.call("getBalance", None).await
    }

    pub async fn get_exchange_rates(&self) -> Result<Value, String> {
        self.call("getExchangeRates", None).await
    }

    pub async fn create_invoice(
        &self,
        amount: &str,
        asset: &str,
        description: Option<&str>,
        paid_btn_name: Option<&str>,
        paid_btn_url: Option<&str>,
    ) -> Result<Value, String> {
        let mut body = serde_json::json!({
            "amount": amount,
            "asset": asset,
        });
        if let Some(d) = description {
            body["description"] = Value::String(d.to_string());
        }
        if let Some(n) = paid_btn_name {
            body["paid_btn_name"] = Value::String(n.to_string());
        }
        if let Some(u) = paid_btn_url {
            body["paid_btn_url"] = Value::String(u.to_string());
        }
        self.call("createInvoice", Some(body)).await
    }

    pub async fn get_invoices(&self, status: Option<&str>) -> Result<Value, String> {
        let mut body = serde_json::json!({});
        if let Some(s) = status {
            body["status"] = Value::String(s.to_string());
        }
        self.call("getInvoices", Some(body)).await
    }

    pub async fn get_invoice(&self, invoice_id: &str) -> Result<Value, String> {
        let body = serde_json::json!({ "invoice_ids": invoice_id });
        self.call("getInvoices", Some(body)).await
    }

    pub async fn create_check(&self, amount: &str, asset: &str) -> Result<Value, String> {
        let body = serde_json::json!({ "amount": amount, "asset": asset });
        self.call("createCheck", Some(body)).await
    }

    /// Transfer funds from the app balance to a user (Crypto Pay `transfer`).
    /// Spends from the bot's balance; the result carries the transfer receipt.
    pub async fn transfer(&self, user_id: i64, asset: &str, amount: &str) -> Result<Value, String> {
        let body = serde_json::json!({ "user_id": user_id, "asset": asset, "amount": amount });
        self.call("transfer", Some(body)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> CryptoPay {
        CryptoPay::with_base_url("test-token".into(), &server.uri())
    }

    /// transfer() posts user_id/asset/amount and returns the result object.
    #[tokio::test]
    async fn transfer_success_posts_expected_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/transfer"))
            .and(body_partial_json(
                json!({"user_id": 42, "asset": "USDT", "amount": "1.5"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": {"transfer_id": 1}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let res = client_for(&server)
            .transfer(42, "USDT", "1.5")
            .await
            .unwrap();
        assert_eq!(res["transfer_id"], 1);
    }

    /// ok:false maps to Err even when `error` is an object, not a string.
    #[tokio::test]
    async fn transfer_api_error_returns_err() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/transfer"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": false,
                "error": {"name": "TRANSFER_FAILED"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        assert!(
            client_for(&server)
                .transfer(42, "USDT", "1.5")
                .await
                .is_err()
        );
    }

    /// get_invoice() posts invoice_ids to getInvoices and unwraps the result.
    #[tokio::test]
    async fn get_invoice_posts_invoice_ids() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getInvoices"))
            .and(body_partial_json(json!({"invoice_ids": "7"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": {"items": [{"invoice_id": 7}]}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let res = client_for(&server).get_invoice("7").await.unwrap();
        assert_eq!(res["items"][0]["invoice_id"], 7);
    }
}
