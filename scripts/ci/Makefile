ifeq ($(GITHUB_ACTIONS),true)
	CONTAINER_TERMINAL := -i
else
	CONTAINER_TERMINAL := -it
endif

export CONTAINER_TERMINAL

regression:
	./regression-test.sh || (echo "Regression test failed" && exit 1)
