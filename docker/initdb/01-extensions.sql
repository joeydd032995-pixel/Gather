-- Runs once at cluster initialization (docker-entrypoint-initdb.d).
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pgcrypto;
