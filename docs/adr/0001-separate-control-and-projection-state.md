# Separate control and projection state

Hoglet stores authoritative Control State in `control.db` and rebuildable
Projections in `projections.db`. Two SQLite databases preserve transactional
locality inside each kind of state while preventing event projection work from
contending with authentication and user-authored configuration; the previous
per-feature database files are migration inputs, not continuing authorities.
