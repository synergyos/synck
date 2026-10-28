CREATE TABLE store_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    repo_id TEXT NOT NULL CHECK (repo_id <> '')
) STRICT;
CREATE TABLE blobs (
    digest BLOB PRIMARY KEY CHECK (length(digest) = 32),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    complete INTEGER NOT NULL CHECK (complete IN (0, 1))
) STRICT;
CREATE TABLE chunks (
    digest BLOB PRIMARY KEY CHECK (length(digest) = 32),
    raw_size INTEGER NOT NULL CHECK (raw_size BETWEEN 1 AND 262144),
    codec TEXT NOT NULL CHECK (codec IN ('RAW', 'ZSTD')),
    payload BLOB NOT NULL,
    CHECK (
        (codec = 'RAW' AND length(payload) = raw_size)
        OR
        (codec = 'ZSTD' AND length(payload) > 0
            AND length(payload) * 100 < raw_size * 95)
    )
) STRICT;
CREATE TABLE blob_chunks (
    blob_digest BLOB NOT NULL CHECK (length(blob_digest) = 32),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    chunk_digest BLOB NOT NULL CHECK (length(chunk_digest) = 32),
    PRIMARY KEY (blob_digest, ordinal),
    FOREIGN KEY (blob_digest) REFERENCES blobs(digest) ON DELETE CASCADE,
    FOREIGN KEY (chunk_digest) REFERENCES chunks(digest)
) STRICT;
CREATE INDEX idx_blob_chunks_chunk_digest ON blob_chunks(chunk_digest);
