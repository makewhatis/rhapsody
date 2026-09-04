# Changelog

## [0.3.4](https://github.com/makewhatis/rhapsody/compare/v0.3.3...v0.3.4) (2026-09-04)


### Features

* **ci:** add a rhapsody@rc Homebrew cask tracking prerelease tags (STUDIO-648) ([#52](https://github.com/makewhatis/rhapsody/issues/52)) ([5f1400b](https://github.com/makewhatis/rhapsody/commit/5f1400b6c25527c6ba80dd7d66e755bc0bf1636f))
* **config,httpapi,mcp,web:** expose reinstate and list-invalidated for the Memory page (STUDIO-689) ([#77](https://github.com/makewhatis/rhapsody/issues/77)) ([d58a20f](https://github.com/makewhatis/rhapsody/commit/d58a20f83219783ce154fa99c9467ace982f515a))
* **config,orchestrator:** give the triage turn a budget it can finish inside (STUDIO-673) ([#66](https://github.com/makewhatis/rhapsody/issues/66)) ([5968561](https://github.com/makewhatis/rhapsody/commit/5968561477c349faa000ea8ed9f98ef7693e19c5))
* **config,orchestrator:** put Teams memory on the hindsight cloud bank, prefetched off the dispatch path (STUDIO-660) ([#58](https://github.com/makewhatis/rhapsody/issues/58)) ([3bf914a](https://github.com/makewhatis/rhapsody/commit/3bf914add86567f44f763c8f71a3b25a2be1f6dd))
* **config:** add a teams.review.mode enum and cut the quorum over (STUDIO-719) ([#91](https://github.com/makewhatis/rhapsody/issues/91)) ([0d7f62f](https://github.com/makewhatis/rhapsody/commit/0d7f62fedabc83b7f7880aed883315f1a767db97))
* **config:** add capabilities registry module (BO-11) ([991f964](https://github.com/makewhatis/rhapsody/commit/991f964678d8ae82cb8a8da926c7b97b6e8c6794))
* **config:** add capabilities registry module (BO-11) ([cbfb707](https://github.com/makewhatis/rhapsody/commit/cbfb707be65ff82ab361293af8dfe18735ccba0e))
* **config:** add the inert Teams toggle, roster and teams.yaml loader (STUDIO-639) ([#47](https://github.com/makewhatis/rhapsody/issues/47)) ([f012bd3](https://github.com/makewhatis/rhapsody/commit/f012bd38a1c8da2e128ecc2c61a33bc3158d9054))
* **config:** default GitHub PR label to "rhapsody" ([#36](https://github.com/makewhatis/rhapsody/issues/36)) ([b06d6d3](https://github.com/makewhatis/rhapsody/commit/b06d6d3f985bb0df033348af256aee59c39bab74))
* **config:** evolvable capabilities registry, atomic writes (review feedback) ([465e57e](https://github.com/makewhatis/rhapsody/commit/465e57e06f67ed9780b773ee1b8350f711c33c4e))
* **config:** resolve Teams profiles from versioned built-ins, with teams show/fork (STUDIO-642) ([#48](https://github.com/makewhatis/rhapsody/issues/48)) ([c379a0d](https://github.com/makewhatis/rhapsody/commit/c379a0dead8d365dfcd6cad972be412a957f1e0c))
* **httpapi,web:** open the team room to the operator (STUDIO-661) ([#59](https://github.com/makewhatis/rhapsody/issues/59)) ([bbffe58](https://github.com/makewhatis/rhapsody/commit/bbffe585a7490611506ffe9d0d42e1fcf8e31d38))
* **httpapi,web:** show a ticket's real state in the Jobs worklist (STUDIO-702) ([#84](https://github.com/makewhatis/rhapsody/issues/84)) ([cfede6d](https://github.com/makewhatis/rhapsody/commit/cfede6d112a501dab2a57084cb296d25ac526c7a))
* **httpapi,web:** steer the review watch set from the authenticated console (STUDIO-722) ([#95](https://github.com/makewhatis/rhapsody/issues/95)) ([b0650f9](https://github.com/makewhatis/rhapsody/commit/b0650f95f759ab5d7c791c556b24b6b79c60e451))
* **httpapi:** capabilities in config CRUD + registry endpoint (BO-13) ([0fb78ba](https://github.com/makewhatis/rhapsody/commit/0fb78baf7abd18d5081b7fec9bc482ed7f1733d4))
* **httpapi:** capabilities in config CRUD + registry endpoint (BO-13) ([be515fe](https://github.com/makewhatis/rhapsody/commit/be515fe152be57e1a17b17c2738bf00efdc09de3))
* **mcp,core:** alias the symphony contracts and ship the rhapsody names (STUDIO-603) ([#44](https://github.com/makewhatis/rhapsody/issues/44)) ([47664d4](https://github.com/makewhatis/rhapsody/commit/47664d42504d0d0359dbc1b20e6d95b07384287f))
* **orchestrator,config:** let the manager act on operator room posts (STUDIO-678) ([#70](https://github.com/makewhatis/rhapsody/issues/70)) ([cd6cc73](https://github.com/makewhatis/rhapsody/commit/cd6cc73ce8d1a2195635afd5b1f047c48b657a68))
* **orchestrator,mcp:** let a teammate post to the team room, host-stamped (STUDIO-653) ([#56](https://github.com/makewhatis/rhapsody/issues/56)) ([c8320f3](https://github.com/makewhatis/rhapsody/commit/c8320f362228bbcdcaed75e21925d954bd674016))
* **orchestrator,tracker:** fan a teammate's handoff out to a review quorum (STUDIO-659) ([#57](https://github.com/makewhatis/rhapsody/issues/57)) ([e03d6c6](https://github.com/makewhatis/rhapsody/commit/e03d6c6f35fa5e671f08aef0255c9a6568e7dcd9))
* **orchestrator,web:** retain the teammate on a job that has left running (STUDIO-735) ([#97](https://github.com/makewhatis/rhapsody/issues/97)) ([f684b73](https://github.com/makewhatis/rhapsody/commit/f684b734cdf4ec53c675925360a19f3cfcda089f))
* **orchestrator,workspace:** dispatch a review run onto a detached PR-head worktree (STUDIO-715) ([#89](https://github.com/makewhatis/rhapsody/issues/89)) ([e83feb6](https://github.com/makewhatis/rhapsody/commit/e83feb611901aa570ea2555d95344be8d7a9b2e7))
* **orchestrator:** add a number-keyed gh PR-state primitive (STUDIO-710) ([#87](https://github.com/makewhatis/rhapsody/issues/87)) ([45ab0d0](https://github.com/makewhatis/rhapsody/commit/45ab0d099643011f7465b199a365342687d181e4))
* **orchestrator:** add a team-scoped manager knowledge read accessor (STUDIO-729) ([#98](https://github.com/makewhatis/rhapsody/issues/98)) ([a2b7420](https://github.com/makewhatis/rhapsody/commit/a2b74209cccb8660f339dbb43fb527441bd17969))
* **orchestrator:** answer a terminal ticket's and a PR's outcome (STUDIO-730) ([#99](https://github.com/makewhatis/rhapsody/issues/99)) ([a7e0c46](https://github.com/makewhatis/rhapsody/commit/a7e0c4654fe1e442d9971dbf8b7f649ed7d12281))
* **orchestrator:** answer an operator's question from the team's own records (STUDIO-731) ([#101](https://github.com/makewhatis/rhapsody/issues/101)) ([861320a](https://github.com/makewhatis/rhapsody/commit/861320a13f1cde81e590b869bf4e37ed3fd51711))
* **orchestrator:** bound the manager's answer so its records survive (STUDIO-732) ([#105](https://github.com/makewhatis/rhapsody/issues/105)) ([9fb486d](https://github.com/makewhatis/rhapsody/commit/9fb486dc2b4cee700d96e0217f4c0fd09845226c))
* **orchestrator:** dispatch credential-liveness preflight (BO-59) ([1dcb4ff](https://github.com/makewhatis/rhapsody/commit/1dcb4ff86e4ecb538cf4b4fb5b4aedccbd278cf9))
* **orchestrator:** dispatch preflight — probe agent-credential liveness (BO-59) ([6063349](https://github.com/makewhatis/rhapsody/commit/60633491491267155d700761b45bf3c5b0cb6394))
* **orchestrator:** fire a re-review from an edge-triggered PR watcher (STUDIO-721) ([#93](https://github.com/makewhatis/rhapsody/issues/93)) ([0833cd8](https://github.com/makewhatis/rhapsody/commit/0833cd8ecf4204f8b2cbef6d5d21a864b328b875))
* **orchestrator:** give a teammate the team room to wake up to (STUDIO-650) ([#54](https://github.com/makewhatis/rhapsody/issues/54)) ([2389bb8](https://github.com/makewhatis/rhapsody/commit/2389bb8b693dca9f2f7fd015399bf52acbe9a68e))
* **orchestrator:** give each teammate a local, inspectable memory (STUDIO-645) ([#51](https://github.com/makewhatis/rhapsody/issues/51)) ([9452a34](https://github.com/makewhatis/rhapsody/commit/9452a34d77ac38199bf0c6f27fa33e607562b761))
* **orchestrator:** guarantee the summon token on a review completion (STUDIO-723) ([#94](https://github.com/makewhatis/rhapsody/issues/94)) ([4ef3b60](https://github.com/makewhatis/rhapsody/commit/4ef3b603831ce5767be4c4bef49523d46249ab91))
* **orchestrator:** hold dispatch until the team has taken the ticket (STUDIO-669) ([#62](https://github.com/makewhatis/rhapsody/issues/62)) ([b1bf433](https://github.com/makewhatis/rhapsody/commit/b1bf433d861e3ea18823634b765b1576b26058ee))
* **orchestrator:** introduce a reviewed PR only from a trusted origin (STUDIO-720) ([#92](https://github.com/makewhatis/rhapsody/issues/92)) ([e2a25a7](https://github.com/makewhatis/rhapsody/commit/e2a25a7e1d904e3a299e8216d176982ee88fdfad))
* **orchestrator:** resolve, prepend, and thread capabilities into turn-1 prompt (BO-12) ([31fcefe](https://github.com/makewhatis/rhapsody/commit/31fcefeb2fefc57e5a5d24320c95d54cdc39ed3c))
* **orchestrator:** resolve, prepend, and thread capabilities into turn-1 prompt (BO-12) ([7c4dff8](https://github.com/makewhatis/rhapsody/commit/7c4dff8a98b715576f5d6032eda8ec0a9ab3f846))
* **orchestrator:** route dispatch to a Teams identity, sync and deterministic (STUDIO-643) ([#49](https://github.com/makewhatis/rhapsody/issues/49)) ([ba57a47](https://github.com/makewhatis/rhapsody/commit/ba57a4705a164aed58ca02254549de862ac7ff94))
* **orchestrator:** triage tickets to a Teams identity off the control loop (STUDIO-644) ([#50](https://github.com/makewhatis/rhapsody/issues/50)) ([6b19bd1](https://github.com/makewhatis/rhapsody/commit/6b19bd164ceb34fc14b3c6d61e4cd761afe0ad4b))
* **orchestrator:** wind a ticketless review run down without a Linear state (STUDIO-716) ([#90](https://github.com/makewhatis/rhapsody/issues/90)) ([031659d](https://github.com/makewhatis/rhapsody/commit/031659d6ddeba471e2daadf7b685917874ad2849))
* **rhapsodyd:** print the room's recent tail in teams show (STUDIO-670) ([#64](https://github.com/makewhatis/rhapsody/issues/64)) ([4be4a87](https://github.com/makewhatis/rhapsody/commit/4be4a87e71556d4d6361a7b2eed5fe7a88974e33))
* **store:** give the review watch set a restart-surviving home (STUDIO-711) ([#88](https://github.com/makewhatis/rhapsody/issues/88)) ([d57114e](https://github.com/makewhatis/rhapsody/commit/d57114e5fd4332af0423912b0533e17d8a231d55))
* **web,httpapi:** show the team in the app — roster, room, memory and an explicit enable flow (STUDIO-652) ([#55](https://github.com/makewhatis/rhapsody/issues/55)) ([b7bc0a4](https://github.com/makewhatis/rhapsody/commit/b7bc0a4fa445ab0d8f23e02427d5b43cfafa1880))
* **web:** add the console app shell, Jobs and Job-detail views (STUDIO-683) ([#74](https://github.com/makewhatis/rhapsody/issues/74)) ([e2d7fe7](https://github.com/makewhatis/rhapsody/commit/e2d7fe72b75c8f7a9076b8b38f18f8c88c748673))
* **web:** add the console design system — tokens, shell and shared components (STUDIO-682) ([#71](https://github.com/makewhatis/rhapsody/issues/71)) ([96f4c5b](https://github.com/makewhatis/rhapsody/commit/96f4c5be4841be63355d1495510116e65bf3fabb))
* **web:** add the console Trace live, failed and attempt-relay states (STUDIO-744) ([#104](https://github.com/makewhatis/rhapsody/issues/104)) ([52b33bb](https://github.com/makewhatis/rhapsody/commit/52b33bba26585c2e474a2d097597e3ce1def03b4))
* **web:** add the console Trace run-detail model (STUDIO-741) ([#100](https://github.com/makewhatis/rhapsody/issues/100)) ([a70082e](https://github.com/makewhatis/rhapsody/commit/a70082eaeda0adf5a95344218446b8cd250b4ea5))
* **web:** add the console Trace watch-tabs rail (STUDIO-745) ([#106](https://github.com/makewhatis/rhapsody/issues/106)) ([1233c02](https://github.com/makewhatis/rhapsody/commit/1233c02e8b01fcc4111ab32a2697623864735165))
* **web:** add the Jobs-home Needs you count and row trace-sparkline (STUDIO-743) ([#103](https://github.com/makewhatis/rhapsody/issues/103)) ([c46fbe5](https://github.com/makewhatis/rhapsody/commit/c46fbe50dc24a59acb8edfba12e0c55712f6e819))
* **web:** bring the console Trace run-detail header up to the prototype (STUDIO-763) ([#112](https://github.com/makewhatis/rhapsody/issues/112)) ([b2e616f](https://github.com/makewhatis/rhapsody/commit/b2e616f956c275b29d921120679fb12cafde7ee5))
* **web:** build the console's Memory page with reversible invalidation (STUDIO-685) ([#75](https://github.com/makewhatis/rhapsody/issues/75)) ([3c75c7f](https://github.com/makewhatis/rhapsody/commit/3c75c7ff360d5ccffd10dd01264f974f3ffa7952))
* **web:** build the manage-team form — teams.yaml as a form (STUDIO-686) ([#76](https://github.com/makewhatis/rhapsody/issues/76)) ([2756945](https://github.com/makewhatis/rhapsody/commit/2756945819ad51c1044c47c1bb799ddb250c8d3c))
* **web:** build the Teams console's typed, grouped, day-paged room (STUDIO-684) ([#73](https://github.com/makewhatis/rhapsody/issues/73)) ([3f0e35c](https://github.com/makewhatis/rhapsody/commit/3f0e35c1092242eacbb402097c8f5cd07aaba1f0))
* **web:** capabilities checklist in the per-project config screen (BO-14) ([5527e38](https://github.com/makewhatis/rhapsody/commit/5527e38458c6e3e96ad3c9108da428f631525038))
* **web:** capabilities checklist in the per-project config screen (BO-14) ([8850a03](https://github.com/makewhatis/rhapsody/commit/8850a034e1034422d952a80e0c782144ed5f12b1))
* **web:** keep a finished run's teammate on the console Trace (STUDIO-746) ([#108](https://github.com/makewhatis/rhapsody/issues/108)) ([a8a442e](https://github.com/makewhatis/rhapsody/commit/a8a442eb4fd648a1c3eaab8cdfbc51654d1b678e))
* **web:** make every Teams field editable in Settings, quorum first (STUDIO-667) ([#61](https://github.com/makewhatis/rhapsody/issues/61)) ([58eb9b0](https://github.com/makewhatis/rhapsody/commit/58eb9b00d5739844cc305d93fa136d4aee1f496e))
* **web:** make the Rhapsody Console the dashboard (STUDIO-687) ([#82](https://github.com/makewhatis/rhapsody/issues/82)) ([e08dbe2](https://github.com/makewhatis/rhapsody/commit/e08dbe2a77eeeca35204a140f8a1076ab25f4ad8))
* **web:** open a working WORKFLOW.md editor from the console's Settings (STUDIO-690) ([#78](https://github.com/makewhatis/rhapsody/issues/78)) ([620711a](https://github.com/makewhatis/rhapsody/commit/620711aed4d53bb688819c10ca314f68072a3efc))
* **web:** reach Tools, Logs and Updates from the console's Settings (STUDIO-691) ([#81](https://github.com/makewhatis/rhapsody/issues/81)) ([34eb4e0](https://github.com/makewhatis/rhapsody/commit/34eb4e0f1f57e0daa5ce951edc41065e2e7e2365))
* **web:** read the manager's room answer back into the console (STUDIO-733) ([#107](https://github.com/makewhatis/rhapsody/issues/107)) ([7fb3187](https://github.com/makewhatis/rhapsody/commit/7fb318738d5cee0dc9d82db750e9801ff5860a94))
* **web:** rebuild the console run detail into the Trace three zones (STUDIO-742) ([#102](https://github.com/makewhatis/rhapsody/issues/102)) ([2d108f4](https://github.com/makewhatis/rhapsody/commit/2d108f49667dd1e649f4f2a8e56e4368d0cbdc29))
* **web:** render agent prose in the console as sanitized markdown (STUDIO-739) ([#96](https://github.com/makewhatis/rhapsody/issues/96)) ([40cb40c](https://github.com/makewhatis/rhapsody/commit/40cb40ce8138b70bb915c0f10c448d6b26b07e69))
* **web:** route the console to the shipped onboarding wizard on a fresh install (STUDIO-692) ([#80](https://github.com/makewhatis/rhapsody/issues/80)) ([f5e0349](https://github.com/makewhatis/rhapsody/commit/f5e0349caacdfd8185ba43dd8211da8903344ad2))


### Bug Fixes

* **agent:** emit SYMPHONY_RUN_ID so dispatched runs can post and retain (STUDIO-675) ([#68](https://github.com/makewhatis/rhapsody/issues/68)) ([7796434](https://github.com/makewhatis/rhapsody/commit/77964344c6b1c03c80365910d8189e31060e660e))
* **docs:** correct rhapsodyd mcp binary name in root CLAUDE.md ([5e7d9f1](https://github.com/makewhatis/rhapsody/commit/5e7d9f1e6186d9c566eb8e76f3005fa1b5d320b8))
* **harness:** teach fake-claude to answer the BO-59 credential probe ([1af2c0e](https://github.com/makewhatis/rhapsody/commit/1af2c0e9d505fa23d7cefe258738c2730477443b))
* **orchestrator:** assign only work the dispatch gate would hold (STUDIO-672) ([#65](https://github.com/makewhatis/rhapsody/issues/65)) ([f4c7643](https://github.com/makewhatis/rhapsody/commit/f4c7643a3b373d8df7d84032268f91501bf531e9))
* **orchestrator:** create the review ticket through the parent's project tracker (STUDIO-677) ([#69](https://github.com/makewhatis/rhapsody/issues/69)) ([d87d343](https://github.com/makewhatis/rhapsody/commit/d87d343e9ce7c327535117d60c712122e8cd549e))
* **orchestrator:** deliver a reopening summons to the run it triggers (STUDIO-649) ([#53](https://github.com/makewhatis/rhapsody/issues/53)) ([db1aad0](https://github.com/makewhatis/rhapsody/commit/db1aad07364873f4e4977834f35874b41ba9f293))
* **orchestrator:** make GitHub-summons enrichment observable and cover fetch→apply (STUDIO-574) ([#37](https://github.com/makewhatis/rhapsody/issues/37)) ([34156b8](https://github.com/makewhatis/rhapsody/commit/34156b8bc53e132d6ba487554565c40e30a87d2b))
* **orchestrator:** resolve the review quorum's PR by head branch when Linear has no attachment (STUDIO-674) ([#67](https://github.com/makewhatis/rhapsody/issues/67)) ([ab5ed16](https://github.com/makewhatis/rhapsody/commit/ab5ed16446420ad577eb79e28edec84e035aee75))
* **orchestrator:** serve /api/v1/state off the control loop so a tick cannot starve it (STUDIO-551) ([#40](https://github.com/makewhatis/rhapsody/issues/40)) ([02f265f](https://github.com/makewhatis/rhapsody/commit/02f265fd9f962599c78f0c803bab2a04c6bc1b9a))
* **orchestrator:** triage every configured project, not the slug-less account tracker (STUDIO-671) ([#63](https://github.com/makewhatis/rhapsody/issues/63)) ([3c41847](https://github.com/makewhatis/rhapsody/commit/3c4184700736e078ced93db03a324530c8d8fef5))
* **web:** label a past-tense handoff heading as How verified, not Notes (STUDIO-764) ([#109](https://github.com/makewhatis/rhapsody/issues/109)) ([080388c](https://github.com/makewhatis/rhapsody/commit/080388cf8f933516ca9889ee653fc3bb16767730))
* **web:** move the console Trace watch-tabs into their own run-scoped zone (STUDIO-766) ([#110](https://github.com/makewhatis/rhapsody/issues/110)) ([e2a7b0e](https://github.com/makewhatis/rhapsody/commit/e2a7b0ec5abb15a7685deaad61c6acaa3598fdad))
* **web:** restore the desktop titlebar chrome the console dropped (STUDIO-701) ([#83](https://github.com/makewhatis/rhapsody/issues/83)) ([18df4ed](https://github.com/makewhatis/rhapsody/commit/18df4ed0f2a0923985e24ba456858a55af7238ed))
* **web:** retry the Teams version gate until the daemon answers (STUDIO-665) ([#60](https://github.com/makewhatis/rhapsody/issues/60)) ([a365214](https://github.com/makewhatis/rhapsody/commit/a365214dfa84acd41198aa89ecdb2e5291fd66cf))
* **web:** route the console's external links through the openExternal seam (STUDIO-765) ([#111](https://github.com/makewhatis/rhapsody/issues/111)) ([fadbf8b](https://github.com/makewhatis/rhapsody/commit/fadbf8bd3eeb28fc6d4b0dafbae624073c903406))
* **web:** route the console's Teams item to the room it was built for (STUDIO-687) ([#79](https://github.com/makewhatis/rhapsody/issues/79)) ([8709e1f](https://github.com/makewhatis/rhapsody/commit/8709e1f41c924ef93773d190f1d3d25c55c472e6))
* **web:** stop the Jobs-list playhead colliding with the Now banner rule (STUDIO-771) ([#113](https://github.com/makewhatis/rhapsody/issues/113)) ([a388e88](https://github.com/makewhatis/rhapsody/commit/a388e881c65b135384835329ddc26bdc5c158328))

## [0.3.3](https://github.com/makewhatis/rhapsody/compare/v0.3.2...v0.3.3) (2026-08-15)


### Bug Fixes

* null attachment fields no longer make a project invisible to the poller (STUDIO-408) ([#28](https://github.com/makewhatis/rhapsody/issues/28)) ([f00bb27](https://github.com/makewhatis/rhapsody/commit/f00bb276d23d0b2449da6039f28feddb201e0713))

## [0.3.2](https://github.com/makewhatis/rhapsody/compare/v0.3.1...v0.3.2) (2026-08-13)


### Features

* **config:** add capabilities field mirroring labels (BO-10) ([131bbc8](https://github.com/makewhatis/rhapsody/commit/131bbc81de80244f05cb957b6083f9b4e7fca3e0))

## [0.3.1](https://github.com/makewhatis/rhapsody/compare/v0.3.0...v0.3.1) (2026-07-26)


### Bug Fixes

* classify a clean exit into a configured review state as completed ([#18](https://github.com/makewhatis/rhapsody/issues/18)) ([7a0edf8](https://github.com/makewhatis/rhapsody/commit/7a0edf8c85a582737b885c7d2aea78f1cfddd4ca))

## [0.3.0](https://github.com/makewhatis/rhapsody/compare/v0.2.2...v0.3.0) (2026-07-26)


### Features

* add Claude Opus 5 as the default model and refresh the model picker ([#16](https://github.com/makewhatis/rhapsody/issues/16)) ([536f1bf](https://github.com/makewhatis/rhapsody/commit/536f1bfc65deca3c28ace74878194c4da0fe9f20))

## [0.2.2](https://github.com/makewhatis/rhapsody/compare/v0.2.1...v0.2.2) (2026-07-21)


### Bug Fixes

* grant core:window:allow-start-dragging so the window can be dragged ([#13](https://github.com/makewhatis/rhapsody/issues/13)) ([1677443](https://github.com/makewhatis/rhapsody/commit/16774439318d1f7c7d1b23b28d639af1ad170f86))

## [0.2.1](https://github.com/makewhatis/rhapsody/compare/v0.2.0...v0.2.1) (2026-07-21)


### Bug Fixes

* wire the native folder/file picker in Settings via tauri-plugin-dialog ([#11](https://github.com/makewhatis/rhapsody/issues/11)) ([6795deb](https://github.com/makewhatis/rhapsody/commit/6795debe6063625efb448a97b703d594024d0bc2))
* write rotating daemon logs to logging.dir (make the Logs path setting real) ([#10](https://github.com/makewhatis/rhapsody/issues/10)) ([fc2d715](https://github.com/makewhatis/rhapsody/commit/fc2d715d5f0fe05c295c3fa2610c5948f241111d))

## [0.2.0](https://github.com/makewhatis/rhapsody/compare/v0.1.0...v0.2.0) (2026-07-18)


### Features

* **crates:** agent abstraction + humanize + fake backend [TRA-208] ([cb1c14a](https://github.com/makewhatis/rhapsody/commit/cb1c14a04323c7051660b3c9e1cd9751eb4e8591))
* **crates:** claude backend — args, parse, billing, mcpinject [TRA-210] ([63595a6](https://github.com/makewhatis/rhapsody/commit/63595a64c26c50b63a6dc4a8c30a769ebc9683d9))
* **crates:** claude runner + fake-claude fixture gate — closes P4 [TRA-212] ([d22e6ec](https://github.com/makewhatis/rhapsody/commit/d22e6ec86dc24b3ee4c4ec628a3a2fd828070f53))
* **crates:** config raw model + Decode (rhapsody-config) [TRA-195] ([95ea5db](https://github.com/makewhatis/rhapsody/commit/95ea5db3d3288bb7e09fe8afaccafaa36040cfd8))
* **crates:** Encode + effective-config goldens — the P1 gate [TRA-201] ([2cecb7c](https://github.com/makewhatis/rhapsody/commit/2cecb7c992094dcd96dc03e27ae8e290a69dcc6f))
* **crates:** file tracker adapter — JSON-backed Tracker parity [TRA-203] ([6c8b287](https://github.com/makewhatis/rhapsody/commit/6c8b287bc88adbd845b91bc942c5f848e097e89c))
* **crates:** full store parity — trait, CRUD, queries, retention, round-trip [TRA-199] ([32938b8](https://github.com/makewhatis/rhapsody/commit/32938b84b51efb07484eece3dbcade765e5c5a17))
* **crates:** harness-fixtures loader + red-on-drift canary [TRA-191] ([768f30c](https://github.com/makewhatis/rhapsody/commit/768f30c98e4fea920fea40483e79b707fe971543))
* **crates:** httpapi server core + web embed + /api/v1/state [TRA-223] ([a109078](https://github.com/makewhatis/rhapsody/commit/a109078a3e2798609490c90560076f7dad43c5f0))
* **crates:** linear adapter — client, query, errors, normalize, tracing [TRA-204] ([bc8603f](https://github.com/makewhatis/rhapsody/commit/bc8603feca8ea53d4f7f52e42d66e37ce18ccdfb))
* **crates:** linear reads — candidates, backlog, by-ids/states, projects, viewer [TRA-205] ([21ae7a9](https://github.com/makewhatis/rhapsody/commit/21ae7a9b18c10699a89c3de71fa49b221f3d9910))
* **crates:** linear writes + stub gate — close the P3 gate [TRA-206] ([c3ea971](https://github.com/makewhatis/rhapsody/commit/c3ea9710a8ea9c3f201773203baa489182c4a42a))
* **crates:** mcp facade — rmcp stdio server + read tools [TRA-224] ([#36](https://github.com/makewhatis/rhapsody/issues/36)) ([1088e8d](https://github.com/makewhatis/rhapsody/commit/1088e8d0b7c1c55742f5c103705b48d39dbbc1e0))
* **crates:** orchestrator control loop + reload + stop + warnings [TRA-219] ([#42](https://github.com/makewhatis/rhapsody/issues/42)) ([60be42b](https://github.com/makewhatis/rhapsody/commit/60be42b0eb45f6df4630a8a35470072b5cbd123a))
* **crates:** orchestrator core state + effective config [TRA-213] ([4028d51](https://github.com/makewhatis/rhapsody/commit/4028d51649736255c945c75f526fdfc6b5bf551e))
* **crates:** orchestrator persistence + snapshot + reads [TRA-216] ([3eff55a](https://github.com/makewhatis/rhapsody/commit/3eff55af4743ba71cf41dcbffb341a19ddc57fcc))
* **crates:** orchestrator retry + recovery + reconcile [TRA-217] ([#33](https://github.com/makewhatis/rhapsody/issues/33)) ([c2e1fbf](https://github.com/makewhatis/rhapsody/commit/c2e1fbf1422a5be157f9d6081b3cafecc8eeec48))
* **crates:** orchestrator selection + claim — eligibility, slots, claim modes [TRA-214] ([951f2c1](https://github.com/makewhatis/rhapsody/commit/951f2c16bd3c767b8da4d72d32a9f1ab59649ae4))
* **crates:** port core domain types (rhapsody-core) [TRA-192] ([6c689e5](https://github.com/makewhatis/rhapsody/commit/6c689e50f99c9c2a5f908423980034d6299c22e4))
* **crates:** Resolve defaults + $VAR/path normalization (rhapsody-config) [TRA-198] ([31725f4](https://github.com/makewhatis/rhapsody/commit/31725f432deb2eeac950018ea0484902cda0259b))
* **crates:** rhapsody-store schema parity + open modes [TRA-197] ([2fa7a27](https://github.com/makewhatis/rhapsody/commit/2fa7a27292c8323e72c6e06361e016bb98acb705))
* **crates:** rhapsodyd daemon assembly + boot gate [TRA-229] ([#48](https://github.com/makewhatis/rhapsody/issues/48)) ([70aa7ff](https://github.com/makewhatis/rhapsody/commit/70aa7ff2a1931a82a6c8b4bd33b348c8b1047353))
* **crates:** scaffold cargo workspace, empty crates, CI (R1) [TRA-187] ([e255129](https://github.com/makewhatis/rhapsody/commit/e25512932afa129db997ad6918578fc367b2de3e))
* **crates:** strict Liquid prompt rendering (rhapsody-config) [TRA-196] ([52b6839](https://github.com/makewhatis/rhapsody/commit/52b6839ec723fdf50a50bb3013cfbcb8c505a38e))
* **crates:** Tracker trait + factory + fake adapter [TRA-202] ([fcfb72b](https://github.com/makewhatis/rhapsody/commit/fcfb72b9d27b8a33946a27666ac39040e8b7cd5a))
* **crates:** Validate + ResolveProjects (rhapsody-config) [TRA-200] ([e57d9a4](https://github.com/makewhatis/rhapsody/commit/e57d9a44bc9f3b1ad21966bdf742c462a0d6f045))
* **crates:** worker + agent updates + workspace GC [TRA-215] ([ccab24a](https://github.com/makewhatis/rhapsody/commit/ccab24a429175ca3652b44802050e1c7e8951796))
* **crates:** WORKFLOW.md loader + save (workflow module) [TRA-193] ([c64ffa8](https://github.com/makewhatis/rhapsody/commit/c64ffa8dab6948de0b25e13e318fa0c2009e1261))
* **crates:** workspace gc + gtguard [TRA-211] ([0f6d951](https://github.com/makewhatis/rhapsody/commit/0f6d951a626fc70d60975dfbe56ce0221c60f5e2))
* **crates:** workspace git layer — repo, safety, sanitize [TRA-207] ([a2b92d4](https://github.com/makewhatis/rhapsody/commit/a2b92d472398c3f6809634e3f6cff94d5cb0e7ef))
* **crates:** workspace manager + hooks + labeler [TRA-209] ([2292a0e](https://github.com/makewhatis/rhapsody/commit/2292a0ec3241a326da076ad0fa0e760933e9be12))
* **desktop:** supervisor + apiproxy + tooldirs + fakedaemon (P7-D2) [TRA-232] ([#46](https://github.com/makewhatis/rhapsody/issues/46)) ([f9eeda4](https://github.com/makewhatis/rhapsody/commit/f9eeda4e048e2ba9c9720a6a1767d525136b81c1))
* **harness:** capture pipeline + committed golden parity fixtures [TRA-190] ([0ecf194](https://github.com/makewhatis/rhapsody/commit/0ecf1942a91f32173fec70c6bfc3071567d23a29))
* **harness:** commit Go-written DB fixture from capture [TRA-194] ([ad34cf5](https://github.com/makewhatis/rhapsody/commit/ad34cf55053eb54bc4f7f23e7373ae556b99422e))
* **harness:** fake-claude + scripted Linear GraphQL stub server [TRA-189] ([bc15795](https://github.com/makewhatis/rhapsody/commit/bc157950af07579ee398e6613eb3492eb51ce506))
* **web:** import Symphony dashboard + wire web CI job [TRA-188] ([2fab907](https://github.com/makewhatis/rhapsody/commit/2fab9072dae917918380f368a11eb27d3ce8044c))


### Bug Fixes

* **crates:** F1 self-review — flag ParseBool, LogSource thread exit, lifetime ctx [TRA-229] ([#49](https://github.com/makewhatis/rhapsody/issues/49)) ([45e0ae2](https://github.com/makewhatis/rhapsody/commit/45e0ae2567a7253484deb57537da0d48f7ff2f76))
* **crates:** preserve web-dist anchor across builds; test Server::serve [TRA-223] ([48ba387](https://github.com/makewhatis/rhapsody/commit/48ba387d6d0dfd6cd3d34e21a47fe71ee0cdb7f7))
* **desktop:** strip hop-by-hop proxy headers + drain healthz body (P7-D2 self-review) [TRA-232] ([#47](https://github.com/makewhatis/rhapsody/issues/47)) ([e1693ac](https://github.com/makewhatis/rhapsody/commit/e1693ac10af594e7363c2a1f2a5adf8fc1324523))
* **dmg:** detach a stale /Volumes/&lt;volname&gt; before packaging ([#75](https://github.com/makewhatis/rhapsody/issues/75)) ([ea89006](https://github.com/makewhatis/rhapsody/commit/ea89006ef72e7245b06e71095fc45120750b39b3))
* **dmg:** package via image+ditto so `make dmg` works on macOS 15+ ([#76](https://github.com/makewhatis/rhapsody/issues/76)) ([90c31bf](https://github.com/makewhatis/rhapsody/commit/90c31bf36c549646539925c4060cc08cf7aca0fb))
* **orchestrator:** daemon auto-parks ticket on HANDOFF (review-gated loop fix) ([#60](https://github.com/makewhatis/rhapsody/issues/60)) ([255463e](https://github.com/makewhatis/rhapsody/commit/255463e84e8fbb6ce7c675461ee2012ba9666fa9))
* **prompt:** review-gated handoff (move ticket to In Review, no self-merge) ([#58](https://github.com/makewhatis/rhapsody/issues/58)) ([2482b0e](https://github.com/makewhatis/rhapsody/commit/2482b0ed0120ddf38dbcb2eb32a0bcbae78ad650))
* **web:** use --tx-2 (not the --muted bg token) for secondary text ([#78](https://github.com/makewhatis/rhapsody/issues/78)) ([ffccaed](https://github.com/makewhatis/rhapsody/commit/ffccaed6a170a7705bfdac2b37fd5cac810dba2b))
