# Watch catch-up ownership

Runtime watcher events resolve each matching registration and use that registration's retained configuration resources. Catch-up completion releases the active slot and queues follow-up work for later admission through `begin_next_catch_up`.
