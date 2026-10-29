use sqlx::PgPool;

#[sqlx::test(migrations = "./migrations")]
async fn smoke_tables_exist(pool: PgPool) {
    for table in [
        "users",
        "tokens",
        "auth_requests",
        "channels",
        "channel_members",
        "invitations",
        "messages",
        "context_entries",
    ] {
        let n: i64 = sqlx::query_scalar(&format!("select count(*) from {table}"))
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("table {table}: {e}"));
        assert_eq!(n, 0);
    }
}
