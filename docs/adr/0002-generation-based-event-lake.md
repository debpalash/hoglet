# Publish the event lake as immutable generations

Queries acquire a leased Published Generation instead of discovering Parquet
files directly. Publication, compaction, retention, and erasure all create a
new generation, so cache validity and file lifetime share one Checkpoint and a
query can never race deletion of the files it is reading.
