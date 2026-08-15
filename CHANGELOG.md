# Changelog

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
