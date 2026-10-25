# The gates (gates/README.md), from the repository root. Their output root comes from the environment
# (FERROFIN_GATES_DIR). IMAGE, the Ferrofin image under test, defaults to the one gates/run names for the
# checked-out commit; PREVIOUS, the previous release's image, makes bench the release A/B gate. Both are
# taken from the command line only, never from the environment, where another tool may have set them:
#   make run-gates IMAGE=ferrofin:v1.4.0-rc PREVIOUS=ghcr.io/mangoleaf/ferrofin:v1.3.2
# There is deliberately no test, check or all target (they would read as the cargo workflow), and a bare
# `make` only lists the targets: a gates session is never started by habit.

.DEFAULT_GOAL := help
cli = $(if $(filter command line,$(origin $(1))),$($(1)))
GATES_RUN = $(strip gates/run $(if $(call cli,IMAGE),--image '$(IMAGE)') $(if $(call cli,PREVIOUS),--previous '$(PREVIOUS)'))

.PHONY: help run-gates run-verify run-adoption run-parity run-bench test-gates-offline

help:
	@echo "make run-gates                 every gate against one image, as one session (gates/run all)"
	@echo "make run-verify|run-adoption|run-parity|run-bench   one gate, as a session"
	@echo "    IMAGE=<image>  the image under test; PREVIOUS=<image>  bench as the release A/B gate"
	@echo "make test-gates-offline        the gates' own tests: no docker, server or fixtures"

run-gates:
	$(GATES_RUN) all

run-verify:
	$(GATES_RUN) verify

run-adoption:
	$(GATES_RUN) adoption

run-parity:
	$(GATES_RUN) parity

run-bench:
	$(GATES_RUN) bench

# The gates' own tests, which need no docker, server or fixtures: shellcheck, the bats tests and the
# standard-library Python tests of every gate. CI's shell-tests job runs this target. gates/bench/run.sh is
# not shellchecked: it carries info-level notes (SC2015, SC2329) from before the gates moved here.
GATE_SCRIPTS = gates/lib.sh gates/run gates/verify/run.sh gates/verify/lib.sh gates/verify/scan-behaviour.sh \
	gates/adoption/lib.sh gates/adoption/run.sh gates/adoption/smoke.sh gates/adoption/build-fixtures.sh \
	gates/parity/lib.sh gates/parity/run.sh gates/parity/reset.sh gates/bench/testdata/build.sh
GATE_TESTS = gates/tests gates/verify/tests gates/adoption/tests gates/parity/tests

test-gates-offline:
	shellcheck -x $(GATE_SCRIPTS)
	shellcheck -x -s bash $(wildcard $(addsuffix /*.bats,$(GATE_TESTS)))
	bats $(GATE_TESTS)
	python3 -m unittest discover -s gates/tests
	python3 -m unittest discover -s gates/adoption/tests
	python3 -m unittest discover -s gates/parity/tests
	python3 -m unittest discover -s gates/bench -p 'test_*.py'
