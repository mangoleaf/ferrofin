
(component
  (core module $m
    (memory (export "memory") 1)
    (global $bump (mut i32) (i32.const 4096))
    (global $events (mut i32) (i32.const 0))
    (func $realloc (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $ret i32)
      global.get $bump
      local.set $ret
      (global.set $bump (i32.add (global.get $bump) (local.get 3)))
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

  (func $descriptor (result $descriptor)
    (canon lift (core func $i "descriptor") (memory $i "memory") string-encoding=utf8))
  (func $default-config (result string)
    (canon lift (core func $i "default-config") (memory $i "memory") string-encoding=utf8))
  (func $tasks (result (list $task))
    (canon lift (core func $i "tasks") (memory $i "memory") string-encoding=utf8))
  (func $run-task (param "task-id" string) (result (result (error string)))
    (canon lift (core func $i "run-task") (memory $i "memory")
      (realloc (core func $i "realloc")) string-encoding=utf8))
  (func $on-event (param "event-name" string) (param "event-json" string)
    (canon lift (core func $i "on-event") (memory $i "memory")
      (realloc (core func $i "realloc")) string-encoding=utf8))

  (export "descriptor" (func $descriptor))
  (export "default-config" (func $default-config))
  (export "tasks" (func $tasks))
  (export "run-task" (func $run-task))
  (export "on-event" (func $on-event))
)
