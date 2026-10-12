Feature: Wiki document lifecycle
  Background:
    Given a repository registered as "demo"

  Scenario: Publish a document for another client to read and find
    When I publish the retention policy
    Then another client reads the exact retention policy
    And the policy is indexed
    And a search finds the retention policy
    And the wiki index links to the policy

  Scenario: Reject replacement without losing the original document
    Given a published retention policy
    When I publish a replacement without overwrite permission
    Then replacement is rejected because the document exists
    And another client reads the exact retention policy
    And a search finds the retention policy
    And the wiki index remains unchanged

  Scenario: Delete a document from the wiki and search index
    Given a published retention policy
    When I delete the retention policy
    Then another client cannot read the policy
    And the policy is absent from search
    And the wiki index does not link to the policy
