Feature: Knowledge stays accurate across wiki operations
  Background:
    Given a repository registered as "demo"

  Scenario: A restarted daemon keeps published knowledge
    When the daemon restarts after a page is published
    Then a new client reads and finds the published page

  Scenario: Restart after a crash restores knowledge behind a stale socket
    When the owned daemon crashes after a page is published
    Then restarting the daemon lets a new client read the page

  Scenario: A second daemon cannot displace the owner
    When another daemon starts on the occupied socket
    Then the second daemon exits and the first client still reads its page

  Scenario: A first client starts an absent daemon
    When a new client publishes without a running daemon
    Then another client reads and finds the published page

  Scenario: External page creation enters search after indexing
    When I create a page outside the wiki tool and index the wiki
    Then the new page is readable and searchable

  Scenario: External edits become searchable through watcher reconciliation
    When I change a published page outside the wiki tool
    Then the changed fact becomes searchable without explicit indexing

  Scenario: External page changes replace indexed facts
    When I change an external page and index the wiki again
    Then search finds the new fact and not the old fact

  Scenario: External page deletion removes stale search results
    When I remove an external page and index the wiki again
    Then the removed page is absent from navigation and search

  Scenario: Repeated indexing keeps the same knowledge
    When I index an unchanged wiki twice
    Then the page remains readable and searchable without duplicate results

  Scenario: Explicit overwrite replaces searchable facts
    When I overwrite a published page
    Then the replacement is readable and only its fact is searchable

  Scenario: A section edit preserves other sections
    When I append guidance under one heading
    Then the other section remains unchanged and the guidance is searchable

  Scenario: An ambiguous edit cannot change a page
    When I replace a repeated phrase without choosing an occurrence
    Then the edit is rejected and the page remains unchanged

  Scenario: Two authors can publish different pages
    When two clients publish different pages together
    Then both pages remain readable and searchable

  Scenario: Two authors can append to one section
    When two clients append different guidance together
    Then both additions remain in the section and in search

  Scenario: One daemon keeps two repositories separate
    When two repositories publish different wiki facts
    Then each repository reads its own page and pinned search stays separate

  Scenario: Readers navigate the current wiki
    When I publish a page in a nested topic
    Then corpus, file, and tree navigation locate the page

  Scenario: Backlinks identify linking pages
    When I publish a target and a page linking to it
    Then backlinks name the linking page and not the target

  Scenario: A reader can cite a page with lines and footnotes
    When I publish a sourced page
    Then reading and search preserve its citation context

  Scenario: Escaping the wiki root cannot change another file
    When I try to publish outside the wiki root
    Then the request is rejected and the outside file stays unchanged

  Scenario: A batch reports each write without rolling back
    When a batch contains a successful write, a rejected write, and another success
    Then the batch reports each outcome and both successful pages remain
