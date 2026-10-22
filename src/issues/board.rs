use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sqlx::{Row, Sqlite, Transaction};

use super::{
    IssueReadError, IssueStore, IssueSummary,
    queries::{authorize_project_read, issue_summary_from_row},
};

#[derive(Clone)]
pub struct BoardCursor {
    rank: i64,
    issue_id: String,
}

impl BoardCursor {
    pub fn decode(value: &str) -> Result<Self, IssueReadError> {
        if !(51..=76).contains(&value.len()) {
            return Err(IssueReadError::InvalidBoardCursor);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| IssueReadError::InvalidBoardCursor)?;
        if URL_SAFE_NO_PAD.encode(&bytes) != value {
            return Err(IssueReadError::InvalidBoardCursor);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| IssueReadError::InvalidBoardCursor)?;
        let (rank, id) = text
            .split_once('/')
            .ok_or(IssueReadError::InvalidBoardCursor)?;
        let parsed: i64 = rank
            .parse()
            .map_err(|_| IssueReadError::InvalidBoardCursor)?;
        if parsed.to_string() != rank
            || uuid::Uuid::parse_str(id)
                .map(|id| id.to_string())
                .ok()
                .as_deref()
                != Some(id)
        {
            return Err(IssueReadError::InvalidBoardCursor);
        }
        Ok(Self {
            rank: parsed,
            issue_id: id.to_owned(),
        })
    }

    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}/{}", self.rank, self.issue_id))
    }
}

#[derive(Clone, Copy)]
pub struct BoardOptions<'a> {
    pub after: Option<&'a BoardCursor>,
    pub page_size: u16,
}

pub struct BoardColumnPage {
    pub items: Vec<IssueSummary>,
    pub total_count: i64,
    pub next_cursor: Option<BoardCursor>,
}

impl IssueStore {
    pub(crate) async fn board_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        actor_user_id: &str,
        project_id: &str,
        status_id: &str,
        options: BoardOptions<'_>,
    ) -> Result<BoardColumnPage, IssueReadError> {
        if !(1..=100).contains(&options.page_size) {
            return Err(IssueReadError::InvalidPageSize);
        }
        authorize_project_read(transaction, actor_user_id, project_id).await?;
        let status = sqlx::query(
            "SELECT board_visible, archived_at FROM statuses WHERE project_id = ? AND id = ?",
        )
        .bind(project_id)
        .bind(status_id)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(IssueReadError::Unavailable)?;
        let visible: i64 = status.try_get("board_visible")?;
        let archived: Option<String> = status.try_get("archived_at")?;
        if !matches!(visible, 0 | 1) {
            return Err(IssueReadError::CorruptData);
        }
        if visible == 0 || archived.is_some() {
            return Err(IssueReadError::Unavailable);
        }
        let total_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issues WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL")
            .bind(project_id).bind(status_id).fetch_one(&mut **transaction).await?;
        let rows = sqlx::query("SELECT issues.id, issues.project_id, issues.issue_number, projects.project_key, issues.issue_type_id, issues.status_id, issues.title, issues.assignee_user_id, issues.creator_user_id, issues.priority, issues.due_date, issues.rank, issues.version, issues.created_at, issues.updated_at FROM issues JOIN projects ON projects.id = issues.project_id WHERE issues.project_id = ? AND issues.status_id = ? AND issues.trashed_at IS NULL AND (issues.rank, issues.id) > (?, ?) ORDER BY issues.rank ASC, issues.id ASC LIMIT ?")
            .bind(project_id).bind(status_id).bind(options.after.map(|cursor| cursor.rank).unwrap_or(i64::MIN))
            .bind(options.after.map(|cursor| cursor.issue_id.as_str()).unwrap_or(""))
            .bind(i64::from(options.page_size) + 1).fetch_all(&mut **transaction).await?;
        let has_more = rows.len() > usize::from(options.page_size);
        let mut items = Vec::with_capacity(rows.len().min(usize::from(options.page_size)));
        for row in rows.into_iter().take(usize::from(options.page_size)) {
            items.push(issue_summary_from_row(&row)?);
        }
        let next_cursor = if has_more {
            items.last().map(|issue| BoardCursor {
                rank: issue.rank,
                issue_id: issue.id.clone(),
            })
        } else {
            None
        };
        Ok(BoardColumnPage {
            items,
            total_count,
            next_cursor,
        })
    }
}
