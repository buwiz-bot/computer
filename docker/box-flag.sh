# shellcheck shell=sh
# The box's on/off rule for a setting, for the shell scripts that read one (entrypoint.sh,
# box-chromium). It is box_common::env::env_bool's rule, so a script and a daemon never read the
# same value two ways: whitespace around the value is trimmed, case is ignored, an empty value is
# the default, and 0, false, off and no are off; anything else is on. POSIX sh, so bash and dash
# can both source it. It defines one function and nothing else.

# flag_on VALUE [DEFAULT]: true when VALUE is on. An empty VALUE (after trimming) is DEFAULT,
# read by the same rule; with no DEFAULT given that is on.
flag_on() {
  _flag_v="${1-}"
  # trim leading, then trailing, whitespace
  _flag_v="${_flag_v#"${_flag_v%%[![:space:]]*}"}"
  _flag_v="${_flag_v%"${_flag_v##*[![:space:]]}"}"
  if [ -z "${_flag_v}" ]; then
    _flag_v="${2:-1}"
  fi
  _flag_v="$(printf '%s' "${_flag_v}" | tr '[:upper:]' '[:lower:]')"
  case "${_flag_v}" in
    0 | false | off | no) return 1 ;;
    *) return 0 ;;
  esac
}
