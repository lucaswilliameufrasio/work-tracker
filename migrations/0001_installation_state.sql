CREATE TABLE installation_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    setup_completed_at TEXT
);

INSERT INTO installation_state (singleton) VALUES (1);
