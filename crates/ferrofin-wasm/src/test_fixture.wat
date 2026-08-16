
(component
  (core module $m
    (memory (export "memory") 1)
    (global $bump (mut i32) (i32.const 4096))
    (global $events (mut i32) (i32.const 0))
    ;; Bump allocator honoring the canonical ABI's align param (arg 2):
    ;; ret = (bump + align - 1) & ~(align - 1).
    (func $realloc (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $ret i32)
      (local.set $ret
        (i32.and
          (i32.add (global.get $bump) (i32.sub (local.get 2) (i32.const 1)))
          (i32.xor (i32.sub (local.get 2) (i32.const 1)) (i32.const -1))))
      (global.set $bump (i32.add (local.get $ret) (local.get 3)))
      local.get $ret)

    ;; ── constant strings ────────────────────────────────────────────
    (data (i32.const 16)  "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeffff") ;; id (36)
    (data (i32.const 64)  "Hello")                                ;; name (5)
    (data (i32.const 80)  "1.2.3")                                ;; version (5)
    (data (i32.const 96)  "Test plugin")                          ;; description (11)
    (data (i32.const 128) "{\"a\":1}")                            ;; default config (7)
    (data (i32.const 144) "greet")                                ;; task id (5)
    (data (i32.const 160) "Greet")                                ;; task name (5)
    (data (i32.const 176) "Says hi")                              ;; task description (7)
    (data (i32.const 192) "Test")                                 ;; task category (4)
    (data (i32.const 208) "kaboom")                               ;; guest error (6)
    (data (i32.const 224) "grow-denied")                          ;; limiter report (11)
    (data (i32.const 240) "grow-allowed")                         ;; limiter report (12)
    (data (i32.const 352) "ok")                                   ;; task 2 id (2)
    (data (i32.const 360) "Okay")                                 ;; task 2 name (4)
    (data (i32.const 368) "Always ok")                            ;; task 2 description (9)
    (data (i32.const 384) "fixture-page")                         ;; page name (12)
    (data (i32.const 448) "fixture.txt")                          ;; transform pattern (11)
    (data (i32.const 464) "AAA")                                  ;; transform search (3)
    (data (i32.const 468) "BBB")                                  ;; transform replace (3)
    (data (i32.const 472) "pong")                                 ;; handler body (4)
    (data (i32.const 488) "Movie")                                ;; scan target (5)
    (data (i32.const 400) "<div data-role=\22page\22>fixture</div>") ;; page html (35)

    ;; descriptor: () -> record of 4 strings (8 i32s at the ret area)
    (func (export "descriptor") (result i32)
      (i32.store (i32.const 512) (i32.const 16))
      (i32.store (i32.const 516) (i32.const 36))
      (i32.store (i32.const 520) (i32.const 64))
      (i32.store (i32.const 524) (i32.const 5))
      (i32.store (i32.const 528) (i32.const 80))
      (i32.store (i32.const 532) (i32.const 5))
      (i32.store (i32.const 536) (i32.const 96))
      (i32.store (i32.const 540) (i32.const 11))
      i32.const 512)

    ;; default-config: () -> string
    (func (export "default-config") (result i32)
      (i32.store (i32.const 576) (i32.const 128))
      (i32.store (i32.const 580) (i32.const 7))
      i32.const 576)

    ;; tasks: () -> list<task-descriptor>; one element of 4 strings at 640
    (func (export "tasks") (result i32)
      (i32.store (i32.const 640) (i32.const 144))
      (i32.store (i32.const 644) (i32.const 5))
      (i32.store (i32.const 648) (i32.const 160))
      (i32.store (i32.const 652) (i32.const 5))
      (i32.store (i32.const 656) (i32.const 176))
      (i32.store (i32.const 660) (i32.const 7))
      (i32.store (i32.const 664) (i32.const 192))
      (i32.store (i32.const 668) (i32.const 4))
      ;; element 2 (contiguous at 672): the always-ok task
      (i32.store (i32.const 672) (i32.const 352))
      (i32.store (i32.const 676) (i32.const 2))
      (i32.store (i32.const 680) (i32.const 360))
      (i32.store (i32.const 684) (i32.const 4))
      (i32.store (i32.const 688) (i32.const 368))
      (i32.store (i32.const 692) (i32.const 9))
      (i32.store (i32.const 696) (i32.const 192))
      (i32.store (i32.const 700) (i32.const 4))
      (i32.store (i32.const 704) (i32.const 640))
      (i32.store (i32.const 708) (i32.const 2))
      i32.const 704)

    ;; config-pages: () -> list<config-page>; one element (name ptr/len,
    ;; content ptr/len, enable-in-main-menu bool) at 960, list pair at 984
    (func (export "config-pages") (result i32)
      (i32.store (i32.const 960) (i32.const 384))
      (i32.store (i32.const 964) (i32.const 12))
      (i32.store (i32.const 968) (i32.const 400))
      (i32.store (i32.const 972) (i32.const 35))
      (i32.store (i32.const 976) (i32.const 0))
      (i32.store (i32.const 984) (i32.const 960))
      (i32.store (i32.const 988) (i32.const 1))
      i32.const 984)

    ;; run-task: (string) -> result<_, string>
    ;; ret area: tag @768, err ptr @772, err len @776
    (func (export "run-task") (param $ptr i32) (param $len i32) (result i32)
      ;; "ok" (len 2) -> ok
      (if (i32.eq (local.get $len) (i32.const 2))
        (then
          (i32.store (i32.const 768) (i32.const 0))
          (i32.store (i32.const 772) (i32.const 0))
          (i32.store (i32.const 776) (i32.const 0))))
      (if (i32.eq (local.get $len) (i32.const 2))
        (then (return (i32.const 768))))

      ;; "count" (len 5) -> err(single digit '0'+events)
      (if (i32.eq (local.get $len) (i32.const 5))
        (then
          (i32.store8 (i32.const 300)
            (i32.add (i32.const 48) (global.get $events)))
          (i32.store (i32.const 768) (i32.const 1))
          (i32.store (i32.const 772) (i32.const 300))
          (i32.store (i32.const 776) (i32.const 1))))
      (if (i32.eq (local.get $len) (i32.const 5))
        (then (return (i32.const 768))))

      ;; len-4 ids dispatch on the first byte
      (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 116)) ;; 't'rap
        (then unreachable))
      (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 108)) ;; 'l'oop
        (then (loop $spin (br $spin))))
      (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 103)) ;; 'g'row
        (then
          ;; ask for +96 pages (6 MiB); -1 means the limiter said no
          (if (i32.eq (memory.grow (i32.const 96)) (i32.const -1))
            (then
              (i32.store (i32.const 768) (i32.const 1))
              (i32.store (i32.const 772) (i32.const 224))
              (i32.store (i32.const 776) (i32.const 11)))
            (else
              (i32.store (i32.const 768) (i32.const 1))
              (i32.store (i32.const 772) (i32.const 240))
              (i32.store (i32.const 776) (i32.const 12))))))
      (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 103))
        (then (return (i32.const 768))))

      ;; "boom" (or anything else) -> err("kaboom")
      (i32.store (i32.const 768) (i32.const 1))
      (i32.store (i32.const 772) (i32.const 208))
      (i32.store (i32.const 776) (i32.const 6))
      i32.const 768)

    ;; metadata-lookup: (item-summary, list) -> result<option<metadata-result>, string>
    ;; item-summary's flat size now exceeds 16, so the canonical ABI passes
    ;; the args INDIRECTLY: one pointer to the arg area. We ignore the args.
    ;; Always ok(none): result tag 0 @832, option tag 0 @840 (payload is
    ;; 8-aligned because metadata-result carries an f64).
    (func (export "metadata-lookup")
      (param i32)
      (result i32)
      (i32.store (i32.const 832) (i32.const 0))
      (i32.store (i32.const 840) (i32.const 0))
      i32.const 832)

    ;; web-transforms: () -> list<web-transform>; one element at 1024
    (func (export "web-transforms") (result i32)
      (i32.store (i32.const 1024) (i32.const 448))  ;; pattern ptr
      (i32.store (i32.const 1028) (i32.const 11))   ;; pattern len
      (i32.store (i32.const 1032) (i32.const 464))  ;; search ptr
      (i32.store (i32.const 1036) (i32.const 3))    ;; search len
      (i32.store (i32.const 1040) (i32.const 468))  ;; replace ptr
      (i32.store (i32.const 1044) (i32.const 3))    ;; replace len
      (i32.store (i32.const 1056) (i32.const 1024)) ;; list ptr
      (i32.store (i32.const 1060) (i32.const 1))    ;; list len
      i32.const 1056)

    ;; declared-egress: () -> list<string>; empty (the fixture fetches
    ;; loopback under the private grant in tests, never public hosts)
    (func (export "declared-egress") (result i32)
      (i32.store (i32.const 1120) (i32.const 0))
      (i32.store (i32.const 1124) (i32.const 0))
      i32.const 1120)

    ;; provider-info: () -> option<provider-descriptor>; none (the fixture
    ;; is not a named provider). Ret area @1184: tag 0.
    (func (export "provider-info") (result i32)
      (i32.store (i32.const 1184) (i32.const 0))
      i32.const 1184)

    ;; scan-targets: () -> list<string>; ["Movie"] so driver tests can
    ;; exercise the analysis pass against this fixture.
    (func (export "scan-targets") (result i32)
      (i32.store (i32.const 1136) (i32.const 488))
      (i32.store (i32.const 1140) (i32.const 5))
      (i32.store (i32.const 1152) (i32.const 1136))
      (i32.store (i32.const 1156) (i32.const 1))
      i32.const 1152)

    ;; scan-media: (item-summary) -> result<_, string>; args indirect (the
    ;; grown item-summary exceeds 16 flats), always ok.
    (func (export "scan-media") (param i32) (result i32)
      (i32.store (i32.const 1168) (i32.const 0))
      (i32.store (i32.const 1172) (i32.const 0))
      (i32.store (i32.const 1176) (i32.const 0))
      i32.const 1168)

    ;; handle-request: (plugin-request) -> plugin-response
    ;; plugin-request flattens to exactly 16 params (the direct-passing
    ;; limit): method p0/p1, path p2/p3, query p4/p5, headers p6/p7,
    ;; body tag/ptr/len p8-p10, user-id tag/ptr/len p11-p13,
    ;; is-admin p14, is-authenticated p15.
    ;; A path starting "/b" ("/boom") traps (containment tests); anything else
    ;; answers 200 "pong" with no headers. Ret area @1088: status,
    ;; headers ptr/len, body ptr/len.
    (func (export "handle-request")
      (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)
      (result i32)
      (if (i32.and
            (i32.ge_u (local.get 3) (i32.const 2))
            (i32.eq (i32.load8_u (i32.add (local.get 2) (i32.const 1))) (i32.const 98)))
        (then unreachable))
      (i32.store (i32.const 1088) (i32.const 200))
      (i32.store (i32.const 1092) (i32.const 0))
      (i32.store (i32.const 1096) (i32.const 0))
      (i32.store (i32.const 1100) (i32.const 472))
      (i32.store (i32.const 1104) (i32.const 4))
      i32.const 1088)

    ;; on-event: (string, string) -> (); "die" traps, else counts
    (func (export "on-event") (param $np i32) (param $nl i32) (param $pp i32) (param $pl i32)
      (if (i32.eq (local.get $nl) (i32.const 3))
        (then unreachable))
      (global.set $events (i32.add (global.get $events) (i32.const 1))))
  )
  (core instance $i (instantiate $m))

  ;; Exported functions may only reference exportable named types, so each
  ;; record is bound to a fresh exported index (the `(export $x ...)` form)
  ;; and the function types reference THAT index.
  (type $descriptor0 (record
    (field "id" string) (field "name" string)
    (field "version" string) (field "description" string)))
  (export $descriptor "plugin-descriptor" (type $descriptor0))
  (type $task0 (record
    (field "id" string) (field "name" string)
    (field "description" string) (field "category" string)))
  (export $task "task-descriptor" (type $task0))
  (type $page0 (record
    (field "name" string) (field "content" (list u8))
    (field "enable-in-main-menu" bool)))
  (export $page "config-page" (type $page0))
  (type $item0 (record
    (field "id" string) (field "name" string) (field "kind" string)
    (field "path" (option string)) (field "parent-id" (option string))
    (field "run-time-ticks" (option s64))
    (field "genres" (list string))
    (field "premiere-date" (option string))
    (field "date-created" (option string))
    (field "community-rating" (option f64))
    (field "production-year" (option s32))
    (field "is-folder" bool)
    (field "played" (option bool))
    (field "is-favorite" (option bool))
    (field "playback-position-ticks" (option s64))))
  (export $item "item-summary" (type $item0))
  (type $req0 (record
    (field "method" string) (field "path" string) (field "query" string)
    (field "headers" (list (tuple string string)))
    (field "body" (option (list u8)))
    (field "user-id" (option string))
    (field "is-admin" bool) (field "is-authenticated" bool)))
  (export $req "plugin-request" (type $req0))
  (type $resp0 (record
    (field "status" u16)
    (field "headers" (list (tuple string string)))
    (field "body" (list u8))))
  (export $resp "plugin-response" (type $resp0))
  (type $pd0 (record
    (field "name" string) (field "supported-kinds" (list string))))
  (export $pd "provider-descriptor" (type $pd0))
  (type $wt0 (record
    (field "path-pattern" string) (field "search" string)
    (field "replace" string)))
  (export $wt "web-transform" (type $wt0))
  (type $meta0 (record
    (field "overview" (option string)) (field "production-year" (option s32))
    (field "community-rating" (option f64)) (field "genres" (list string))
    (field "provider-ids" (list (tuple string string)))
    (field "tagline" (option string)) (field "studios" (list string))
    (field "tags" (list string)) (field "official-rating" (option string))
    (field "end-date" (option string))))
  (export $meta "metadata-result" (type $meta0))

  (func $descriptor (result $descriptor)
    (canon lift (core func $i "descriptor") (memory $i "memory") string-encoding=utf8))
  (func $default-config (result string)
    (canon lift (core func $i "default-config") (memory $i "memory") string-encoding=utf8))
  (func $tasks (result (list $task))
    (canon lift (core func $i "tasks") (memory $i "memory") string-encoding=utf8))
  (func $config-pages (result (list $page))
    (canon lift (core func $i "config-pages") (memory $i "memory") string-encoding=utf8))
  (func $web-transforms (result (list $wt))
    (canon lift (core func $i "web-transforms") (memory $i "memory") string-encoding=utf8))
  (func $provider-info (result (option $pd))
    (canon lift (core func $i "provider-info") (memory $i "memory") string-encoding=utf8))
  (func $scan-targets (result (list string))
    (canon lift (core func $i "scan-targets") (memory $i "memory") string-encoding=utf8))
  (func $scan-media (param "item" $item) (result (result (error string)))
    (canon lift (core func $i "scan-media") (memory $i "memory")
      (realloc (core func $i "realloc")) string-encoding=utf8))
  (func $declared-egress (result (list string))
    (canon lift (core func $i "declared-egress") (memory $i "memory") string-encoding=utf8))
  (func $handle-request (param "request" $req) (result $resp)
    (canon lift (core func $i "handle-request") (memory $i "memory")
      (realloc (core func $i "realloc")) string-encoding=utf8))
  (func $run-task (param "task-id" string) (result (result (error string)))
    (canon lift (core func $i "run-task") (memory $i "memory")
      (realloc (core func $i "realloc")) string-encoding=utf8))
  (func $on-event (param "event-name" string) (param "event-json" string)
    (canon lift (core func $i "on-event") (memory $i "memory")
      (realloc (core func $i "realloc")) string-encoding=utf8))
  (func $metadata-lookup
    (param "item" $item) (param "provider-ids" (list (tuple string string)))
    (result (result (option $meta) (error string)))
    (canon lift (core func $i "metadata-lookup") (memory $i "memory")
      (realloc (core func $i "realloc")) string-encoding=utf8))

  (export "descriptor" (func $descriptor))
  (export "default-config" (func $default-config))
  (export "tasks" (func $tasks))
  (export "config-pages" (func $config-pages))
  (export "web-transforms" (func $web-transforms))
  (export "provider-info" (func $provider-info))
  (export "scan-targets" (func $scan-targets))
  (export "scan-media" (func $scan-media))
  (export "declared-egress" (func $declared-egress))
  (export "handle-request" (func $handle-request))
  (export "run-task" (func $run-task))
  (export "on-event" (func $on-event))
  (export "metadata-lookup" (func $metadata-lookup))
)
