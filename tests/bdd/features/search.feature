Feature: Union search stays accurate across corpora and checkouts
  Background:
    Given a repository registered as "demo"

  Scenario: A missing scratch corpus does not degrade search
    When the repository declares a scratch corpus whose folder is absent
    Then union search finds the wiki page without a failed pass for the scratch corpus

  Scenario: A weak leader of another corpus ranks below stronger local pages
    When another corpus holds one page that weakly matches the query
    Then union search ranks both local pages above the other corpus's page

  Scenario: A fresh worktree searches its committed wiki at once
    When I add a git worktree of a committed and indexed repository
    Then the first search from the worktree finds the page and the index converges
