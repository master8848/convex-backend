//go:build wasip1

// A fixture Go guest module implementing the Convex WASM ABI, used by the
// wasm_runner integration tests. Built with:
//
//	GOOS=wasip1 GOARCH=wasm go build -buildmode=c-shared -o go_guest.wasm .
package main

import (
	"encoding/json"
	"unsafe"
)

// Host functions (module "env"), matching wasm_runner/src/abi.rs.

//go:wasmimport env __convex_input_length
func inputLength() int32

//go:wasmimport env __convex_input_load
func inputLoad(offset int32, dest unsafe.Pointer, length int32)

//go:wasmimport env __convex_call_data_load
func callDataLoad(offset int32, dest unsafe.Pointer, length int32)

//go:wasmimport env __convex_output_set
func outputSet(ptr unsafe.Pointer, length int32)

//go:wasmimport env __convex_error_set
func errorSet(ptr unsafe.Pointer, length int32)

//go:wasmimport env __convex_log
func convexLog(ptr unsafe.Pointer, length int32)

//go:wasmimport env __convex_now_ms
func nowMs() int64

//go:wasmimport env __convex_random_bytes
func randomBytes(dest unsafe.Pointer, length int32)

//go:wasmimport env __convex_db_get
func dbGet(ptr unsafe.Pointer, length int32) int64

//go:wasmimport env __convex_db_insert
func dbInsert(ptr unsafe.Pointer, length int32) int64

//go:wasmimport env __convex_db_count
func dbCount(ptr unsafe.Pointer, length int32) int64

//go:wasmimport env __convex_db_query
func dbQuery(ptr unsafe.Pointer, length int32) int64

// Read the full input payload.
func readInput() []byte {
	length := inputLength()
	if length <= 0 {
		return []byte{}
	}
	buf := make([]byte, length)
	inputLoad(0, unsafe.Pointer(&buf[0]), length)
	return buf
}

// Read a database result from host call data.
func readCallData(offset, length uint32) []byte {
	buf := make([]byte, length)
	if length > 0 {
		callDataLoad(int32(offset), unsafe.Pointer(&buf[0]), int32(length))
	}
	return buf
}

// Call a database host function with JSON args, returning the unwrapped
// "ok" value or an error.
func dbCall(name string, args any) (json.RawMessage, error) {
	argBytes, err := json.Marshal(args)
	if err != nil {
		return nil, err
	}
	var result int64
	switch name {
	case "__convex_db_get":
		result = dbGet(unsafe.Pointer(&argBytes[0]), int32(len(argBytes)))
	case "__convex_db_insert":
		result = dbInsert(unsafe.Pointer(&argBytes[0]), int32(len(argBytes)))
	case "__convex_db_count":
		result = dbCount(unsafe.Pointer(&argBytes[0]), int32(len(argBytes)))
	case "__convex_db_query":
		result = dbQuery(unsafe.Pointer(&argBytes[0]), int32(len(argBytes)))
	default:
		return nil, errUnknownOp
	}
	if result < 0 {
		return nil, errSystem
	}
	offset := uint32(result >> 32)
	length := uint32(result)
	var envelope struct {
		OK  *json.RawMessage `json:"ok"`
		Err *string          `json:"err"`
	}
	if err := json.Unmarshal(readCallData(offset, length), &envelope); err != nil {
		return nil, err
	}
	if envelope.OK != nil {
		return *envelope.OK, nil
	}
	if envelope.Err != nil {
		return nil, dbError(*envelope.Err)
	}
	return nil, errSystem
}

type dbError string

func (e dbError) Error() string { return string(e) }

var (
	errUnknownOp = dbError("unknown database operation")
	errSystem    = dbError("database system error")
)

// The function implementations.

func goEcho(value string) (string, error) {
	return value, nil
}

func goAdd(a, b float64) (float64, error) {
	return a + b + float64(nowMs()), nil
}

func goBump() (float64, error) {
	count, err := dbCall("__convex_db_count", map[string]any{"table": "counters"})
	if err != nil {
		return 0, err
	}
	var countF float64
	if err := json.Unmarshal(count, &countF); err != nil {
		return 0, err
	}
	next := countF + 1
	value, _ := json.Marshal(map[string]any{"count": next})
	if _, err := dbCall("__convex_db_insert", map[string]any{
		"table": "counters",
		"value": json.RawMessage(value),
	}); err != nil {
		return 0, err
	}
	return next, nil
}

func goRandom() ([]float64, error) {
	buf := make([]byte, 8)
	randomBytes(unsafe.Pointer(&buf[0]), int32(len(buf)))
	out := make([]float64, len(buf))
	for i, b := range buf {
		out[i] = float64(b)
	}
	return out, nil
}

// Input payload: {"function": string, "args": array}.
type payload struct {
	Function string          `json:"function"`
	Args     json.RawMessage `json:"args"`
}

// The generated module exports.

//go:wasmexport __convex_run
func convexRun() int32 {
	input := readInput()
	var p payload
	if err := json.Unmarshal(input, &p); err != nil {
		writeError("invalid input payload")
		return 1
	}
	var args []json.RawMessage
	if err := json.Unmarshal(p.Args, &args); err != nil {
		writeError("args must be an array")
		return 1
	}
	var result any
	var err error
	switch p.Function {
	case "echo":
		var value string
		err = json.Unmarshal(args[0], &value)
		result = value
	case "add":
		var a, b float64
		if err = json.Unmarshal(args[0], &a); err == nil {
			err = json.Unmarshal(args[1], &b)
		}
		result, err = goAdd(a, b)
	case "bump":
		result, err = goBump()
	case "random":
		result, err = goRandom()
	default:
		writeError("function not found: " + p.Function)
		return 1
	}
	if err != nil {
		writeError(err.Error())
		return 1
	}
	out, err := json.Marshal(result)
	if err != nil {
		writeError(err.Error())
		return 1
	}
	outputSet(unsafe.Pointer(&out[0]), int32(len(out)))
	return 0
}

//go:wasmexport __convex_functions
func convexFunctions() int32 {
	descriptors := []map[string]string{
		{"name": "echo", "type": "query"},
		{"name": "add", "type": "query"},
		{"name": "bump", "type": "mutation"},
		{"name": "random", "type": "query"},
	}
	out, err := json.Marshal(descriptors)
	if err != nil {
		return 1
	}
	outputSet(unsafe.Pointer(&out[0]), int32(len(out)))
	return 0
}

func writeError(message string) {
	bytes := []byte(message)
	errorSet(unsafe.Pointer(&bytes[0]), int32(len(bytes)))
}

func main() {}
