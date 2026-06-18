// Copyright (C) 2017 Ecma International.  All rights reserved.
// This code is governed by the BSD license found in the LICENSE file.
// Heavily adapted for the interp VM:
//  - No `typeof JSON` (JSON is a namespace, not a global value)
//  - No `Object.prototype.toString.call` (Object is a namespace)
//  - No `Array.prototype.map.call` (Array is a namespace)
//  - No `instanceof` in Test262Error (interp constructs without it)

function isNegativeZero(value) {
  return value === 0 && 1 / value === -Infinity;
}

function formatIdentityFreeValue(value) {
  switch (value === null ? 'null' : typeof value) {
    case 'string':
      return '"' + value + '"';
    case 'number':
      if (isNegativeZero(value)) return '-0';
    case 'boolean':
    case 'undefined':
    case 'null':
      return String(value);
  }
}

function formatSimpleValue(value) {
  var basic = formatIdentityFreeValue(value);
  if (basic) return basic;
  try {
    return String(value);
  } catch (err) {
    return '[object]';
  }
}

function assert(mustBeTrue, message) {
  if (mustBeTrue === true) {
    return;
  }

  if (message === undefined) {
    message = 'Expected true but got ' + assert._toString(mustBeTrue);
  }
  throw new Test262Error(message);
}

assert._isSameValue = function (a, b) {
  if (a === b) {
    return a !== 0 || 1 / a === 1 / b;
  }
  return a !== a && b !== b;
};

assert.sameValue = function (actual, expected, message) {
  try {
    if (assert._isSameValue(actual, expected)) {
      return;
    }
  } catch (error) {
    throw new Test262Error(message + ' (_isSameValue operation threw) ' + error);
    return;
  }

  if (message === undefined) {
    message = '';
  } else {
    message += ' ';
  }

  message += 'Expected SameValue(' + assert._toString(actual) + ', ' + assert._toString(expected) + ') to be true';

  throw new Test262Error(message);
};

assert.notSameValue = function (actual, unexpected, message) {
  if (!assert._isSameValue(actual, unexpected)) {
    return;
  }

  if (message === undefined) {
    message = '';
  } else {
    message += ' ';
  }

  message += 'Expected SameValue(' + assert._toString(actual) + ', ' + assert._toString(unexpected) + ') to be false';

  throw new Test262Error(message);
};

assert.throws = function (expectedErrorConstructor, func, message) {
  var expectedName, actualName;
  if (typeof func !== "function") {
    throw new Test262Error('assert.throws requires two arguments: the error constructor and a function to run');
  }
  if (message === undefined) {
    message = '';
  } else {
    message += ' ';
  }

  try {
    func();
  } catch (thrown) {
    if (typeof thrown !== 'object' || thrown === null) {
      message += 'Thrown value was not an object!';
      throw new Test262Error(message);
    } else if (thrown.constructor !== expectedErrorConstructor) {
      expectedName = expectedErrorConstructor.name;
      actualName = thrown.constructor.name;
      if (expectedName === actualName) {
        message += 'Expected a ' + expectedName + ' but got a different error constructor with the same name';
      } else {
        message += 'Expected a ' + expectedName + ' but got a ' + actualName;
      }
      throw new Test262Error(message);
    }
    return;
  }

  message += 'Expected a ' + expectedErrorConstructor.name + ' to be thrown but no exception was thrown at all';
  throw new Test262Error(message);
};

function arrayFormatHelper(arrayLike) {
  var parts = [];
  for (var i = 0; i < arrayLike.length; i++) {
    parts.push(String(arrayLike[i]));
  }
  return "[" + parts.join(", ") + "]";
}

assert.compareArray = function (actual, expected, message) {
  message = message === undefined ? '' : message;

  if (actual === null || actual === undefined || (typeof actual !== 'object' && typeof actual !== 'function')) {
    assert(false, "Actual argument [" + actual + "] shouldn't be primitive. " + String(message));
  } else if (expected === null || expected === undefined || (typeof expected !== 'object' && typeof expected !== 'function')) {
    assert(false, "Expected argument [" + expected + "] shouldn't be primitive. " + String(message));
  }
  var result = compareArray(actual, expected);
  if (result) return;

  assert(false, "Actual " + arrayFormatHelper(actual) + " and expected " + arrayFormatHelper(expected) + " should have the same contents. " + String(message));
};

function compareArray(a, b) {
  if (b.length !== a.length) {
    return false;
  }
  for (var i = 0; i < a.length; i++) {
    if (!assert._isSameValue(b[i], a[i])) {
      return false;
    }
  }
  return true;
}

assert._toString = formatSimpleValue;
