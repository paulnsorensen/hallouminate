Feature: Repository registration
  Scenario: Register a repository for later processes
    Given an empty repository
    When I register the repository as "demo"
    Then registration succeeds
    And a later process validates the registered repository
    And the wiki has the initial index

  Scenario: Reject accidental replacement of a repository
    Given a repository registered as "demo"
    When I register the repository as "other"
    Then registration is rejected because the configuration exists
    And the repository configuration and wiki remain unchanged

  Scenario: Rename a repository without replacing its wiki
    Given a repository registered as "demo"
    When I explicitly rename the repository to "renamed"
    Then registration succeeds
    And a later process sees the repository named "renamed"
    And the wiki remains unchanged
