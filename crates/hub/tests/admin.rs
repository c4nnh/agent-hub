use ah_hub::admin::set_plan;
use sqlx::PgPool;
use uuid::Uuid;

async fn plan_of(pool: &PgPool, email: &str) -> String {
    sqlx::query_scalar("select plan from users where email = $1")
        .bind(email)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn set_plan_updates_user(pool: PgPool) {
    sqlx::query("insert into users (id, email, name) values ($1, 'a@x.io', 'A')")
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(plan_of(&pool, "a@x.io").await, "free");

    set_plan(&pool, "a@x.io", "paid").await.unwrap();
    assert_eq!(plan_of(&pool, "a@x.io").await, "paid");

    set_plan(&pool, "a@x.io", "free").await.unwrap();
    assert_eq!(plan_of(&pool, "a@x.io").await, "free");
}

#[sqlx::test]
async fn set_plan_unknown_email_errors(pool: PgPool) {
    let err = set_plan(&pool, "nobody@x.io", "paid").await.unwrap_err();
    assert!(err.contains("nobody@x.io"), "{err}");
}
