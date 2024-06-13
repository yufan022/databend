python3 tests/udf/udf_server.py
make unit-test
make stateless-test
bash ./scripts/ci/ci-run-sqllogic-tests.sh base
bash ./scripts/ci/ci-run-sqllogic-tests.sh standalone
bash ./scripts/ci/ci-run-sqllogic-tests.sh query
bash ./scripts/ci/ci-run-sqllogic-tests.sh tpcds
bash ./scripts/ci/ci-run-sqllogic-tests.sh tpch
bash ./scripts/ci/ci-run-sqllogic-tests-cluster.sh base
bash ./scripts/ci/ci-run-sqllogic-tests-cluster.sh cluster
bash ./scripts/ci/ci-run-sqllogic-tests-cluster.sh query
bash ./scripts/ci/ci-run-sqllogic-tests-cluster.sh tpcds
bash ./scripts/ci/ci-run-sqllogic-tests-cluster.sh tpch
bash ./scripts/ci/ci-run-sqllogic-tests-native.sh explain_native
