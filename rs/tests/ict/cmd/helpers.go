package cmd

import (
	"bytes"
	"fmt"
	"os/exec"
	"strings"

	"github.com/schollz/closestmatch"
)

var RED = "\033[1;31m"
var GREEN = "\033[1;32m"
var CYAN = "\033[0;36m"
var NC = "\033[0m"

// Max number of results displayed in the fuzzy search.
var FUZZY_MATCHES_COUNT = 7

// see https://github.com/schollz/closestmatch
var FUZZY_SEARCH_BAG_SIZES = []int{2, 3, 4}
var COLOCATE_TEST_SUFFIX = "_colocate"

func find_matching_target(all_targets []string, target string, is_fuzzy_search bool) (string, string, error) {
	if is_fuzzy_search {
		closest_matches := get_closest_target_matches(all_targets, target)
		if len(closest_matches) == 0 {
			return "", "", fmt.Errorf("\nNo fuzzy matches for target `%s` were found", target)
		} else if len(closest_matches) == 1 {
			msg := fmt.Sprintf("Target `%s` doesn't exist, a single fuzzy match `%s` was found and will be used ...\n", target, closest_matches[0])
			return closest_matches[0], msg, nil
		} else {
			return "", "", fmt.Errorf("\nMultiple fuzzy matches were found for `%s`:\n%s", target, strings.Join(closest_matches, "\n"))
		}
	} else {
		substring_matches := find_substring_matches_in_array(all_targets, target)
		if len(substring_matches) == 0 {
			return "", "", fmt.Errorf("\nNone of the %d existing targets matches the substring `%s`.\nTry fuzzy match: 'ict test %s --fuzzy'", len(all_targets), target, target)
		} else if len(substring_matches) == 1 {
			msg := fmt.Sprintf("Target `%s` doesn't exist. However, a single substring match `%s` was found and will be used  ...\n", target, substring_matches[0])
			return substring_matches[0], msg, nil
		} else if len(substring_matches) == 2 {
			// If there are two target matches and one of them is the colocate version of another,
			// then we use a non-colocate one by default.
			non_colocate_test := filter(substring_matches, func(s string) bool {
				return !strings.Contains(s, COLOCATE_TEST_SUFFIX)
			})
			colocate_test := filter(substring_matches, func(s string) bool {
				return !strings.Contains(s, COLOCATE_TEST_SUFFIX)
			})
			if len(colocate_test) == 1 && len(non_colocate_test) == 1 && strings.Contains(colocate_test[0], non_colocate_test[0]) {
				msg := fmt.Sprintf("Target `%s` doesn't exist. However, a single substring match (for non-colocated test) `%s` was found and will be used  ...\n", target, non_colocate_test)
				return non_colocate_test[0], msg, nil
			} else {
				return "", "", fmt.Errorf("\nTarget `%s` doesn't exist. However, the following substring matches found:\n%s", target, strings.Join(substring_matches, "\n"))
			}
		} else {
			return "", "", fmt.Errorf("\nTarget `%s` doesn't exist. However, the following substring matches found:\n%s", target, strings.Join(substring_matches, "\n"))
		}
	}
}

func filter(vs []string, f func(string) bool) []string {
	filtered := make([]string, 0)
	for _, v := range vs {
		if f(v) {
			filtered = append(filtered, v)
		}
	}
	return filtered
}

func any_contains_substring(vs []string, v string) bool {
	for _, s := range vs {
		if strings.Contains(s, v) {
			return true
		}
	}
	return false
}

func any_equals(vs []string, v string) bool {
	for _, s := range vs {
		if s == v {
			return true
		}
	}
	return false
}

func find_substring_matches_in_array(vs []string, substr string) []string {
	matches := filter(vs, func(s string) bool {
		return strings.Contains(s, substr)
	})
	return matches
}

func get_all_system_test_targets() ([]string, error) {
	command := []string{"bazel", "query", "tests(//rs/tests/...)"}
	queryCmd := exec.Command(command[0], command[1:]...)
	outputBuffer := &bytes.Buffer{}
	stdErrBuffer := &bytes.Buffer{}
	queryCmd.Stdout = outputBuffer
	queryCmd.Stderr = stdErrBuffer
	if err := queryCmd.Run(); err != nil {
		return []string{}, fmt.Errorf("bazel command: [%s] failed: %s", strings.Join(command, " "), stdErrBuffer.String())
	}
	cmdOutput := strings.Split(outputBuffer.String(), "\n")
	all_targets := filter(cmdOutput, func(s string) bool {
		return len(s) > 0
	})
	return all_targets, nil
}

func make_fully_qualified_target(target string) (string, error) {
	if strings.Contains(target, ":") {
		return target, nil
	}
	all_targets, err := get_all_testnets()
	if err != nil {
		return "", nil
	}
	target_prefix := strings.Split(all_targets[0], ":")
	return target_prefix[0] + ":" + target, nil
}

func get_all_testnets() ([]string, error) {
	command := []string{"bazel", "query", "attr(tags, 'dynamic_testnet', tests(//rs/tests/...))"}
	queryCmd := exec.Command(command[0], command[1:]...)
	outputBuffer := &bytes.Buffer{}
	stdErrBuffer := &bytes.Buffer{}
	queryCmd.Stdout = outputBuffer
	queryCmd.Stderr = stdErrBuffer
	if err := queryCmd.Run(); err != nil {
		return []string{}, fmt.Errorf("bazel command: [%s] failed: %s", strings.Join(command, " "), stdErrBuffer.String())
	}
	cmdOutput := strings.Split(outputBuffer.String(), "\n")
	all_targets := filter(cmdOutput, func(s string) bool {
		return len(s) > 0
	})
	return all_targets, nil
}

func get_closest_target_matches(all_targets []string, target string) []string {
	closest_matches := closestmatch.New(all_targets, FUZZY_SEARCH_BAG_SIZES).ClosestN(target, FUZZY_MATCHES_COUNT)
	return filter(closest_matches, func(s string) bool {
		return len(s) > 0
	})
}
