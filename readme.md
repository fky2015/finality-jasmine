# Finality Jasmine

A finality gadget for Substrate using a modified version of [Jasmine](https://github.com/fky2015/Jasmine).

## Design notes

- Block generation module: block generation in Jasmine.
  - How to trigger: rather than `on-slot`, it gets triggered with `on-event`.
    - When became leader, the replica is allowed to propose in-between blocks.
    - When gather a QC, the replica is allowed to propose key blocks. (And exits.)
  - How to know if a replica is the leader?
    - The consensus module maintain leader, and expose the state to the generation module.
- Block consensus module: block vote in Jasmine (on key blocks).
  - How to know if a replica is the leader?
    - round-robin + round number.
  - How to track blocks' state, if its in prevote or precommit?
    - track it inside the consensus module.
  - Where to store the QC?
  <!-- TODO: -->
    - justification or in the block itself?

