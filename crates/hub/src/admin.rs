use sqlx::PgPool;

/// Sets a user's plan (`free` or `paid`). Errors if no user has that email.
pub async fn set_plan(pool: &PgPool, email: &str, plan: &str) -> Result<(), String> {
    let result = sqlx::query("update users set plan = $1 where email = $2")
        .bind(plan)
        .bind(email)
        .execute(pool)
        .await
        .map_err(|e| format!("database error: {e}"))?;
    if result.rows_affected() == 0 {
        return Err(format!("no user with email {email}"));
    }
    Ok(())
}
