use chrono::{DateTime, Utc};
use sqlx::{FromRow, sqlite::SqlitePool};

#[derive(Debug, Clone, FromRow)]
pub struct RepoRow {
    pub id: String,
    pub name: String,
    pub org: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct UserRow {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
    pub password_hash: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct RepoAccessRow {
    pub repo_id: String,
    pub user_id: String,
    pub role: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct AuthTokenRow {
    pub token: String,
    pub user_id: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    pub async fn new(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = SqlitePool::connect(database_url).await?;
        Ok(Self { pool })
    }

    pub async fn run_migrations(&self) -> Result<(), sqlx::Error> {
        sqlx::query(include_str!("../migrations/001_init.sql"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // Repo operations
    pub async fn list_repos(&self) -> Result<Vec<RepoRow>, sqlx::Error> {
        sqlx::query_as::<_, RepoRow>(
            "SELECT id, name, org, created_at FROM repos ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// List repos that a user has access to
    pub async fn list_repos_for_user(&self, user_id: &str) -> Result<Vec<RepoRow>, sqlx::Error> {
        sqlx::query_as::<_, RepoRow>(
            r#"
            SELECT r.id, r.name, r.org, r.created_at
            FROM repos r
            INNER JOIN repo_access ra ON r.id = ra.repo_id
            WHERE ra.user_id = ?
            ORDER BY r.created_at DESC
            "#
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get_repo(&self, id: &str) -> Result<Option<RepoRow>, sqlx::Error> {
        sqlx::query_as::<_, RepoRow>("SELECT id, name, org, created_at FROM repos WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn create_repo(
        &self,
        id: &str,
        name: &str,
        org: &str,
    ) -> Result<RepoRow, sqlx::Error> {
        let now = Utc::now();
        sqlx::query("INSERT INTO repos (id, name, org, created_at) VALUES (?, ?, ?, ?)")
            .bind(id)
            .bind(name)
            .bind(org)
            .bind(now.to_rfc3339())
            .execute(&self.pool)
            .await?;

        Ok(RepoRow {
            id: id.to_string(),
            name: name.to_string(),
            org: org.to_string(),
            created_at: now,
        })
    }

    pub async fn delete_repo(&self, id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM repos WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    // User operations
    pub async fn get_user(&self, id: &str) -> Result<Option<UserRow>, sqlx::Error> {
        sqlx::query_as::<_, UserRow>(
            "SELECT id, email, name, password_hash, created_at FROM users WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn get_user_by_email(&self, email: &str) -> Result<Option<UserRow>, sqlx::Error> {
        sqlx::query_as::<_, UserRow>(
            "SELECT id, email, name, password_hash, created_at FROM users WHERE email = ?",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn create_user(
        &self,
        id: &str,
        email: &str,
        name: Option<&str>,
        password_hash: &str,
    ) -> Result<UserRow, sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO users (id, email, name, password_hash, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(email)
        .bind(name)
        .bind(password_hash)
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(UserRow {
            id: id.to_string(),
            email: email.to_string(),
            name: name.map(String::from),
            password_hash: password_hash.to_string(),
            created_at: now,
        })
    }

    // Access control
    pub async fn get_user_role(
        &self,
        user_id: &str,
        repo_id: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        let result = sqlx::query_as::<_, (String,)>(
            "SELECT role FROM repo_access WHERE user_id = ? AND repo_id = ?",
        )
        .bind(user_id)
        .bind(repo_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(result.map(|r| r.0))
    }

    pub async fn set_user_role(
        &self,
        user_id: &str,
        repo_id: &str,
        role: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT OR REPLACE INTO repo_access (user_id, repo_id, role) VALUES (?, ?, ?)")
            .bind(user_id)
            .bind(repo_id)
            .bind(role)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // Auth tokens
    pub async fn create_token(
        &self,
        token: &str,
        user_id: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO auth_tokens (token, user_id, expires_at) VALUES (?, ?, ?)")
            .bind(token)
            .bind(user_id)
            .bind(expires_at.to_rfc3339())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn verify_token(&self, token: &str) -> Result<Option<UserRow>, sqlx::Error> {
        sqlx::query_as::<_, UserRow>(
            "SELECT u.id, u.email, u.name, u.password_hash, u.created_at
             FROM users u
             INNER JOIN auth_tokens t ON u.id = t.user_id
             WHERE t.token = ? AND t.expires_at > datetime('now')",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn delete_token(&self, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM auth_tokens WHERE token = ?")
            .bind(token)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
