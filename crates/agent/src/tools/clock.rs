//! `current_time`:模型不知道現在的時間,要問。

use jiff::tz::TimeZone;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::{Tool, ToolContext, ToolError};

/// `current_time` 工具。時間從 `ToolContext` 的時鐘拿,測試和 eval 可以固定。
pub struct CurrentTime;

// 參數。注意:欄位上的 `///` 會變成 schema 的 description、送給模型看,所以寫給模型;
// 給讀程式的人的說明用 `//`(schemars 不會讀)。
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CurrentTimeArgs {
    /// IANA 時區名稱,例如 "Asia/Taipei"。省略時用 UTC。
    pub timezone: Option<String>,
}

impl Tool for CurrentTime {
    type Args = CurrentTimeArgs;

    fn name(&self) -> &str {
        "current_time"
    }

    fn description(&self) -> &str {
        "取得現在的日期、時間與星期幾。你自己不知道現在的時間;問題跟「今天」「現在」「幾天後」有關時,先用這個工具。"
    }

    // trait 寫的是 `fn call(..) -> impl Future<..> + Send`,實作可以直接寫 `async fn`:
    // compiler 會檢查這個 async fn 產生的 future 真的是 `Send`。
    async fn call(&self, args: CurrentTimeArgs, ctx: &ToolContext) -> Result<String, ToolError> {
        let zone = match args.timezone.as_deref() {
            None => TimeZone::UTC,
            Some(name) => time_zone(name)?,
        };
        let now = ctx.clock.now().to_zoned(zone);
        // `Zoned` 的 Display:`2026-11-14T10:30:00+08:00[Asia/Taipei]`;`%A` 是英文的星期幾。
        // 星期幾從 `now`(已經換成當地時間)算,不是從 UTC 算。
        Ok(format!("{now}, {}", now.strftime("%A")))
    }
}

/// IANA 時區名稱最長的也遠小於這個數。
const MAX_ZONE_NAME: usize = 64;

/// 名稱是模型給的(不可信),先檢查形狀,再查 jiff 的時區資料庫。
// jiff 只拿名稱去比對它掃描 zoneinfo 目錄得到的清單,`../` 本來就讀不到目錄外的檔案;
// 這裡再檢查一次,是不依賴第三方的內部行為(換了實作也一樣安全),而且錯誤訊息更清楚。
fn time_zone(name: &str) -> Result<TimeZone, ToolError> {
    let well_formed = name.len() <= MAX_ZONE_NAME
        && name.split('/').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'))
        });
    if !well_formed {
        return Err(ToolError::InvalidArgs(
            r#"timezone must be an IANA name such as "Asia/Taipei""#.into(),
        ));
    }
    // 通過上面的檢查,名稱最多 64 個字元、沒有怪字元,放進錯誤訊息是安全的。
    TimeZone::get(name).map_err(|_| ToolError::InvalidArgs(format!("unknown time zone {name:?}")))
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use uuid::Uuid;

    use super::*;
    use crate::tool::{Clock, ErasedTool};

    /// 2026-11-14 02:30 UTC 是星期六;台北已經上午,洛杉磯還是星期五晚上。
    fn ctx() -> ToolContext {
        ToolContext {
            run_id: Uuid::nil(),
            call_id: "toolu_1".into(),
            clock: Clock::Fixed("2026-11-14T02:30:00Z".parse::<Timestamp>().unwrap()),
        }
    }

    async fn ask(args: &str) -> Result<String, ToolError> {
        CurrentTime.call_json(args, &ctx()).await
    }

    #[tokio::test]
    async fn without_a_time_zone_it_answers_in_utc() {
        assert_eq!(
            ask("{}").await,
            Ok("2026-11-14T02:30:00+00:00[UTC], Saturday".to_string())
        );
        // JSON 的 null 也是「沒給」。
        assert_eq!(
            ask(r#"{"timezone": null}"#).await,
            Ok("2026-11-14T02:30:00+00:00[UTC], Saturday".to_string())
        );
    }

    #[tokio::test]
    async fn a_named_time_zone_shifts_the_time() {
        assert_eq!(
            ask(r#"{"timezone": "Asia/Taipei"}"#).await,
            Ok("2026-11-14T10:30:00+08:00[Asia/Taipei], Saturday".to_string())
        );
    }

    #[tokio::test]
    async fn the_weekday_follows_the_time_zone() {
        // 星期幾要用當地的日期算;用 UTC 算會說成星期六。
        assert_eq!(
            ask(r#"{"timezone": "America/Los_Angeles"}"#).await,
            Ok("2026-11-13T18:30:00-08:00[America/Los_Angeles], Friday".to_string())
        );
    }

    #[tokio::test]
    async fn an_unknown_time_zone_is_an_argument_error() {
        assert_eq!(
            ask(r#"{"timezone": "Mars/Olympus_Mons"}"#).await,
            Err(ToolError::InvalidArgs(
                r#"unknown time zone "Mars/Olympus_Mons""#.into()
            ))
        );
        // 64 個字元:格式檢查放行,查不到才拒絕(對照下一個測試的 65 個字元)。
        let longest = "A".repeat(64);
        assert_eq!(
            ask(&format!(r#"{{"timezone": "{longest}"}}"#)).await,
            Err(ToolError::InvalidArgs(format!(
                r#"unknown time zone "{longest}""#
            )))
        );
    }

    #[tokio::test]
    async fn anything_shaped_unlike_a_zone_name_is_rejected_before_lookup() {
        let too_long = "A".repeat(65);
        for name in [
            "../../etc/passwd",
            "/etc/localtime",
            "Asia//Taipei",
            "Asia/Taipei/",
            "",
            "Asia/Tai pei",
            too_long.as_str(),
        ] {
            let args = serde_json::json!({ "timezone": name }).to_string();
            assert_eq!(
                ask(&args).await,
                Err(ToolError::InvalidArgs(
                    r#"timezone must be an IANA name such as "Asia/Taipei""#.into()
                )),
                "{name:?}"
            );
        }
    }

    #[tokio::test]
    async fn every_legal_name_shape_is_accepted() {
        // 一段、三段、含 `_`、`-`、`+`、數字。
        for name in [
            "UTC",
            "America/Argentina/Buenos_Aires",
            "America/Port-au-Prince",
            "Etc/GMT+5",
        ] {
            let args = serde_json::json!({ "timezone": name }).to_string();
            let answer = ask(&args).await;
            assert!(
                answer
                    .as_ref()
                    .is_ok_and(|a| a.contains(&format!("[{name}]"))),
                "{name}: {answer:?}"
            );
        }
    }

    #[test]
    fn the_definition_sent_to_the_model_is_locked() {
        insta::assert_json_snapshot!(CurrentTime.def());
    }
}
