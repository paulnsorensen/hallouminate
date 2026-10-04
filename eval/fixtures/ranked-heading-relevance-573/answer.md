# Storage Questions

## How does the cold cache recover after a node restart?

The cold cache replays its journal and checks the final checkpoint before serving requests. This prevents incomplete writes from becoming visible.
