# Gather — PostgreSQL 16 with pgvector, for local development and the
# optional VPS replication target. The pgvector/pgvector image is the
# official postgres image plus a prebuilt pgvector extension.
FROM pgvector/pgvector:pg16

# Enable the extension in every database created by initdb (the daemon's
# migrations also run CREATE EXTENSION IF NOT EXISTS as a belt-and-braces).
COPY docker/initdb/01-extensions.sql /docker-entrypoint-initdb.d/01-extensions.sql

# Conservative tuning for a desktop-daemon workload (small, long-running,
# latency-sensitive). Values are safe on a 2 GB container limit.
CMD ["postgres", \
     "-c", "shared_buffers=256MB", \
     "-c", "effective_cache_size=768MB", \
     "-c", "maintenance_work_mem=128MB", \
     "-c", "work_mem=16MB", \
     "-c", "max_connections=32", \
     "-c", "wal_compression=on", \
     "-c", "log_min_duration_statement=250ms"]
