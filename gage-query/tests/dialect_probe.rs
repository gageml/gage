use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::*;

#[tokio::test(flavor = "current_thread")]
async fn probe_dialect() {
    let config = SessionConfig::new().set_str("datafusion.sql_parser.dialect", "PostgreSQL");
    let state = SessionStateBuilder::new()
        .with_config(config)
        .with_default_features()
        .build();
    let ctx = SessionContext::new_with_state(state);
    datafusion_functions_json::register_all(&mut ctx.clone()).unwrap();
    eprintln!(
        "DIALECT = {:?}",
        ctx.state().config().options().sql_parser.dialect
    );
    ctx.sql("CREATE TABLE e (raw TEXT) AS VALUES ('{\"message\":{\"content\":[{\"type\":\"thinking\"}]}}')").await.unwrap().collect().await.unwrap();
    let sql = "SELECT 1 FROM e WHERE e.raw->'message'->'content'->0->>'type' = 'thinking'";
    match ctx.sql(sql).await {
        Ok(df) => match df.collect().await {
            Ok(b) => eprintln!("OK: {} rows", b.iter().map(|r| r.num_rows()).sum::<usize>()),
            Err(e) => eprintln!("EXEC ERR: {e}"),
        },
        Err(e) => eprintln!("PLAN ERR: {e}"),
    }
}
